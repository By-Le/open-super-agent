"""聊天软件（假的 Telegram Bot API）和手机推送（假的推送服务，HTTPS 自签证书）。

- 添加连接、配对码、配对成功后在“Telegram”里和 Bot 说话，回复发回 Telegram；
- 确认请求变成带按钮的消息，点按钮（callback_query）就允许，卡片改成结果；/new；停用、删除；
- 没有页面正被看着时推送到订阅的设备，内容用测试自己的私钥解得开；404 / 410 的订阅删掉。
"""
import base64
import datetime
import ipaddress
import json
import os
import ssl
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

from conftest import Server, Socket, idle, until
from fakellm_http import Call

TOKEN = "123456789:" + "A" * 35
OWNER, STRANGER = 777, 888


class FakeTelegram:
    """Bot API：getUpdates 从 updates 里取；发出去的都记在 sent 里。"""

    def __init__(self):
        self.lock = threading.Lock()
        self.updates: list[dict] = []
        self.sent: list[tuple[str, dict]] = []
        self.next_update = 1
        self.next_msg = 100
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def push(self, **u):
        with self.lock:
            u["update_id"] = self.next_update
            self.next_update += 1
            self.updates.append(u)

    def say(self, chat: int, text: str):
        self.push(message={"message_id": self.next_update, "chat": {"id": chat, "type": "private"},
                           "from": {"id": chat, "first_name": "Test", "last_name": "User"}, "text": text})

    def press(self, chat: int, data: str):
        self.push(callback_query={"id": f"cq{self.next_update}", "data": data, "message": {"chat": {"id": chat}}})

    def of(self, method: str, chat: int | None = None) -> list[dict]:
        with self.lock:
            return [b for m, b in self.sent if m == method and (chat is None or str(b.get("chat_id")) == str(chat))]

    def texts(self, chat: int) -> list[str]:
        return [b.get("text", "") for b in self.of("sendMessage", chat)]

    def _handler(self):
        fake = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _json(self, d):
                body = json.dumps(d).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get("content-length") or 0)) or b"{}")
                token, method = self.path.split("/")[1][3:], self.path.split("/")[2]
                if token != TOKEN:
                    return self._json({"ok": False, "error_code": 401, "description": "Unauthorized"})
                if method == "getUpdates":
                    end = time.time() + 1
                    while time.time() < end:
                        with fake.lock:
                            ups = [u for u in fake.updates if u["update_id"] >= (body.get("offset") or 0)]
                        if ups:
                            return self._json({"ok": True, "result": ups})
                        time.sleep(0.03)
                    return self._json({"ok": True, "result": []})
                with fake.lock:
                    fake.sent.append((method, body))
                if method == "getMe":
                    return self._json({"ok": True, "result": {"id": 4242, "is_bot": True, "username": "osa_test_bot"}})
                if method == "sendMessage":
                    with fake.lock:
                        fake.next_msg += 1
                        n = fake.next_msg
                    return self._json({"ok": True, "result": {"message_id": n}})
                return self._json({"ok": True, "result": True})

        return H


# ---------- 假的推送服务（HTTPS） ----------
def _cert(tmp: Path) -> tuple[Path, ssl.SSLContext]:
    now = datetime.datetime.now(datetime.timezone.utc)
    ca_key = ec.generate_private_key(ec.SECP256R1())
    ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "osa test ca")])
    ca = (x509.CertificateBuilder().subject_name(ca_name).issuer_name(ca_name).public_key(ca_key.public_key())
          .serial_number(x509.random_serial_number()).not_valid_before(now - datetime.timedelta(days=1))
          .not_valid_after(now + datetime.timedelta(days=30))
          .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
          .add_extension(x509.SubjectKeyIdentifier.from_public_key(ca_key.public_key()), critical=False)
          .add_extension(x509.KeyUsage(digital_signature=True, key_cert_sign=True, crl_sign=True, content_commitment=False,
                                       key_encipherment=False, data_encipherment=False, key_agreement=False,
                                       encipher_only=False, decipher_only=False), critical=True)
          .sign(ca_key, hashes.SHA256()))
    key = ec.generate_private_key(ec.SECP256R1())
    leaf = (x509.CertificateBuilder().subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")]))
            .issuer_name(ca_name).public_key(key.public_key()).serial_number(x509.random_serial_number())
            .not_valid_before(now - datetime.timedelta(days=1)).not_valid_after(now + datetime.timedelta(days=30))
            .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost"), x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]), critical=False)
            .add_extension(x509.ExtendedKeyUsage([ExtendedKeyUsageOID.SERVER_AUTH]), critical=False)
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
            .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()), critical=False)
            .add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
            .sign(ca_key, hashes.SHA256()))
    pem = serialization.Encoding.PEM
    (tmp / "ca.pem").write_bytes(ca.public_bytes(pem))
    (tmp / "leaf.pem").write_bytes(leaf.public_bytes(pem))
    (tmp / "leaf.key").write_bytes(key.private_bytes(pem, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(tmp / "leaf.pem", tmp / "leaf.key")
    return tmp / "ca.pem", ctx


class FakePush:
    def __init__(self, ctx: ssl.SSLContext):
        self.got: list[tuple[str, dict, bytes]] = []
        self.status: dict[str, int] = {}
        fake = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("content-length") or 0))
                fake.got.append((self.path, {k.lower(): v for k, v in self.headers.items()}, body))
                code = fake.status.get(self.path, 201)
                self.send_response(code)
                self.send_header("content-length", "0")
                self.end_headers()

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.httpd.socket = ctx.wrap_socket(self.httpd.socket, server_side=True)
        self.url = f"https://localhost:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()


def b64u(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def unb64u(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def point(k) -> bytes:
    return k.public_key().public_bytes(serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint)


def decrypt(body: bytes, priv, auth: bytes) -> dict:
    """RFC 8291 的另一头：手机用自己的私钥和 auth 解开。"""
    salt, rs, idlen = body[:16], int.from_bytes(body[16:20], "big"), body[20]
    as_pub = body[21:21 + idlen]
    assert rs == 4096 and idlen == 65
    secret = priv.exchange(ec.ECDH(), ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(), as_pub))
    ikm = HKDF(hashes.SHA256(), 32, auth, b"WebPush: info\x00" + point(priv) + as_pub).derive(secret)
    cek = HKDF(hashes.SHA256(), 16, salt, b"Content-Encoding: aes128gcm\x00").derive(ikm)
    nonce = HKDF(hashes.SHA256(), 12, salt, b"Content-Encoding: nonce\x00").derive(ikm)
    plain = AESGCM(cek).decrypt(nonce, body[21 + idlen:], None)
    assert plain.endswith(b"\x02")
    return json.loads(plain[:-1])


@pytest.fixture(scope="module")
def tg():
    return FakeTelegram()


@pytest.fixture(scope="module")
def pushsvc():
    tmp = Path(tempfile.mkdtemp(prefix="osa-push-"))
    ca, ctx = _cert(tmp)
    p = FakePush(ctx)
    p.ca = ca
    return p


@pytest.fixture(scope="module")
def srv(llm, tg, pushsvc):
    s = Server(llm, env={"OSA_TELEGRAM_API": tg.url, "SSL_CERT_FILE": str(pushsvc.ca)})
    yield s
    s.stop()


@pytest.fixture(scope="module")
def channel(srv, tg):
    """一个配对好的 Telegram 连接，绑定 Bot「Tele」的私聊。"""
    bot = srv.make_bot("Tele")
    r = srv.http.post("/api/channels", json={"kind": "telegram", "fields": {"token": "bad"}, "chat_id": bot["chat_id"]})
    assert r.status_code == 400 and "token" in r.json()["detail"]
    ch = srv.post("/api/channels", {"kind": "telegram", "fields": {"token": TOKEN}, "chat_id": bot["chat_id"]})
    assert ch["name"] == "@osa_test_bot" and ch["kind"] == "telegram" and not ch["paired"]
    assert len(ch["pair_code"]) == 6 and ch["pair_link"] == f"https://t.me/osa_test_bot?start={ch['pair_code']}"
    dup = srv.http.post("/api/channels", json={"kind": "telegram", "fields": {"token": TOKEN}, "chat_id": bot["chat_id"]})
    assert dup.status_code == 400
    until(lambda: tg.of("setMyCommands"), what="连上 Telegram")
    assert until(lambda: [c for c in srv.get("/api/channels")["channels"] if c["state"] == "connected"], what="状态是已连接")

    tg.say(OWNER, "000000")
    until(lambda: "配对码不对。" in tg.texts(OWNER), what="配对码不对")
    tg.say(OWNER, f"/start {ch['pair_code']}")
    until(lambda: any("配对成功" in t for t in tg.texts(OWNER)), what="配对成功")
    c = until(lambda: [c for c in srv.get("/api/channels")["channels"] if c["paired"]], what="接口里显示配对了")[0]
    assert c["peer_name"] == "Test User" and "pair_code" not in c
    return {"bot": bot, "id": ch["id"]}


def test_chat_through_telegram(srv, fake, tg, channel):
    bot = channel["bot"]
    tg.say(STRANGER, "我是别人")
    fake.script("router:Tele", "你好，我是 **Tele**")
    tg.say(OWNER, "你好")
    until(lambda: any("我是 <b>Tele</b>" in t for t in tg.texts(OWNER)), what="回复发回 Telegram（HTML 格式）")
    user = srv.find(bot["chat_id"], role="user", text="你好")
    assert user and user[0]["meta"].get("via") == "telegram"
    assert not srv.find(bot["chat_id"], text="我是别人")      # 没配对的人说的话不理
    assert not tg.texts(STRANGER) or all("配对" not in t for t in tg.texts(STRANGER)[1:])
    assert tg.of("sendChatAction", OWNER)                     # 说话前显示“正在输入”

    tg.say(OWNER, "/new")
    until(lambda: any("已开始新话题" in t for t in tg.texts(OWNER)), what="/new")
    assert srv.find(bot["chat_id"], kind="reset")
    tg.say(OWNER, "/help")
    until(lambda: any("/stop" in t for t in tg.texts(OWNER)), what="/help")


def test_approval_button(srv, fake, tg, channel):
    until(lambda: any(a["name"] == "demo" and a.get("state") == "connected" for a in srv.get("/api/apps")["apps"]),
          timeout=30, what="假应用连上")
    fake.script("router:Tele", Call("dispatch", {"goal": "记一条笔记：周五交周报"}), "这就记")
    fake.script("worker", Call("demo__create_note", {"text": "周五交周报"}), "记好了")
    tg.say(OWNER, "帮我记一条笔记")
    card = until(lambda: [b for b in tg.of("sendMessage", OWNER) if b.get("reply_markup", {}).get("inline_keyboard")],
                 what="确认卡片（带按钮）")[0]
    buttons = card["reply_markup"]["inline_keyboard"][0]
    assert "需要你确认" in card["text"] and "create_note" in card["text"]
    ok = next(b for b in buttons if b["callback_data"].endswith(":ok"))
    tg.press(STRANGER, ok["callback_data"])
    until(lambda: any(b.get("text") == "没有权限" for b in tg.of("answerCallbackQuery")), what="别人点按钮没用")
    tg.press(OWNER, ok["callback_data"])
    until(lambda: any("已允许" in (b.get("text") or "") for b in tg.of("answerCallbackQuery")), what="点按钮的提示")
    until(lambda: any("已允许" in b["text"] for b in tg.of("editMessageText", OWNER)), what="卡片改成结果")
    until(lambda: any("完成" in t and "记好了" in t for t in tg.texts(OWNER)), what="任务结果发回 Telegram")
    idle(srv)


def test_word_command_and_settings(srv, fake, tg, channel):
    fake.script("router:Tele", Call("dispatch", {"goal": "再记一条"}), "好")
    fake.script("worker", Call("demo__create_note", {"text": "再记一条"}), "又记好了")
    tg.say(OWNER, "再记一条")
    card = until(lambda: [b for b in tg.of("sendMessage", OWNER) if b.get("reply_markup", {}).get("inline_keyboard")
                          and "再记一条" in b["text"]], what="第二张卡片")[0]
    aid = card["reply_markup"]["inline_keyboard"][0][0]["callback_data"].split(":")[1]
    tg.say(OWNER, f"拒绝 #{aid}")
    until(lambda: any("已拒绝" in b["text"] for b in tg.of("editMessageText", OWNER)), what="打字拒绝")
    idle(srv)

    bad = srv.http.put("/api/channels/settings", json={"public_url": "ftp://x"})
    assert bad.status_code == 400
    assert srv.put("/api/channels/settings", {"public_url": "https://osa.example.com/"})["public_url"] == "https://osa.example.com"


def test_disable_and_delete(srv, tg, channel):
    cid = channel["id"]
    c = srv.patch(f"/api/channels/{cid}", {"enabled": False})
    assert c["state"] == "off" and not c["enabled"]
    c = srv.patch(f"/api/channels/{cid}", {"enabled": True})
    assert c["enabled"]
    re = srv.post(f"/api/channels/{cid}/pair")
    assert len(re["pair_code"]) == 6 and not re["paired"]
    assert srv.http.post("/api/channels/99999/pair").status_code == 404
    assert srv.delete(f"/api/channels/{cid}") == {"ok": True}
    assert not srv.get("/api/channels")["channels"]


def test_push(srv, fake, pushsvc):
    info = srv.get("/api/push")
    assert info["count"] == 0 and len(unb64u(info["key"])) == 65
    assert srv.http.post("/api/push/test").status_code == 400
    assert srv.http.post("/api/push/subscribe", json={"subscription": {"endpoint": "http://x"}}).status_code == 400
    priv, auth = ec.generate_private_key(ec.SECP256R1()), os.urandom(16)
    keys = {"p256dh": b64u(point(priv)), "auth": b64u(auth)}
    assert srv.post("/api/push/subscribe", {"subscription": {"endpoint": f"{pushsvc.url}/w/phone", "keys": keys}})["count"] == 1
    pushsvc.status["/w/gone"] = 410
    assert srv.post("/api/push/subscribe", {"subscription": {"endpoint": f"{pushsvc.url}/w/gone", "keys": keys}})["count"] == 2

    bot = srv.make_bot("Pusher")
    fake.script("router:Pusher", "推送的回复")
    ws = Socket(srv)
    ws.send({"type": "visibility", "visible": False})   # 页面切到后台：没有页面正被看着
    time.sleep(0.2)
    ws.say(bot["chat_id"], "推一下")
    hit = until(lambda: [g for g in pushsvc.got if g[0] == "/w/phone" and decrypt(g[2], priv, auth)["body"] == "推送的回复"],
                what="推送到手机")[0]
    msg = decrypt(hit[2], priv, auth)
    assert msg["title"] == "Pusher" and msg["url"] == f"/?chat={bot['chat_id']}" and msg["tag"] == f"chat-{bot['chat_id']}"
    h = hit[1]
    assert h["content-encoding"] == "aes128gcm" and h["ttl"] == "86400" and h["authorization"].startswith("vapid t=")
    assert h["topic"] and len(h["topic"]) <= 32
    ws.close()
    until(lambda: srv.get("/api/push")["count"] == 1, what="410 的订阅删掉了")
    r = srv.post("/api/push/test")
    assert r["results"] == ["ok"]
    assert srv.post("/api/push/unsubscribe", {"endpoint": f"{pushsvc.url}/w/phone"})["count"] == 0
    idle(srv)
