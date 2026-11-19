"""要联网看的事件来源：日历（ICS）、文件夹、收邮件（IMAP）。本机起假的日历网站和假的 IMAP 服务器（TLS，测试自己签的证书），
两个版本跑同一套。天气、地震、价格的接口地址是写死的公网地址，只有单元测试（Python 的 tests/test_source_*.py，Rust 的 sources/*.rs）。"""
import datetime as dt
import socketserver
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
from cryptography.x509.oid import NameOID

from conftest import idle, until

PAGES: dict[str, tuple[str, bytes]] = {}
MAILS: list[bytes] = []


def _site():
    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            ctype, body = PAGES.get(self.path, ("text/plain", b"nope"))
            self.send_response(200 if self.path in PAGES else 404)
            self.send_header("content-type", ctype)
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


# ---------- 测试用的证书：一个 CA，签一张 127.0.0.1 的证书 ----------
def _certs(tmp: Path) -> tuple[Path, Path, Path]:
    now = dt.datetime.now(dt.timezone.utc)
    ca_key = ec.generate_private_key(ec.SECP256R1())
    ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "osa test CA")])
    ca = (x509.CertificateBuilder().subject_name(ca_name).issuer_name(ca_name).public_key(ca_key.public_key())
          .serial_number(1).not_valid_before(now - dt.timedelta(days=1)).not_valid_after(now + dt.timedelta(days=2))
          .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
          .add_extension(x509.SubjectKeyIdentifier.from_public_key(ca_key.public_key()), critical=False)
          .add_extension(x509.KeyUsage(digital_signature=True, key_cert_sign=True, crl_sign=True, content_commitment=False,
                                       key_encipherment=False, data_encipherment=False, key_agreement=False,
                                       encipher_only=False, decipher_only=False), critical=True)
          .sign(ca_key, hashes.SHA256()))
    key = ec.generate_private_key(ec.SECP256R1())
    import ipaddress
    leaf = (x509.CertificateBuilder().subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "127.0.0.1")]))
            .issuer_name(ca_name).public_key(key.public_key()).serial_number(2)
            .not_valid_before(now - dt.timedelta(days=1)).not_valid_after(now + dt.timedelta(days=2))
            .add_extension(x509.SubjectAlternativeName([x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]), critical=False)
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
            .add_extension(x509.ExtendedKeyUsage([x509.oid.ExtendedKeyUsageOID.SERVER_AUTH]), critical=False)
            .add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
            .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()), critical=False)
            .sign(ca_key, hashes.SHA256()))
    ca_pem, crt, kpem = tmp / "ca.pem", tmp / "leaf.pem", tmp / "leaf.key"
    ca_pem.write_bytes(ca.public_bytes(serialization.Encoding.PEM))
    crt.write_bytes(leaf.public_bytes(serialization.Encoding.PEM))
    kpem.write_bytes(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
    return ca_pem, crt, kpem


# ---------- 假的 IMAP 服务器：只认 osa 会发的那几条命令，收件箱是 MAILS ----------
def _imap(crt: Path, key: Path):
    ctx = ssl.create_default_context(ssl.Purpose.CLIENT_AUTH)
    ctx.load_cert_chain(crt, key)

    class H(socketserver.StreamRequestHandler):
        def setup(self):
            self.request = ctx.wrap_socket(self.request, server_side=True)
            super().setup()

        def send(self, s: str | bytes):
            self.wfile.write(s.encode() if isinstance(s, str) else s)
            self.wfile.flush()

        def handle(self):
            self.send("* OK fake IMAP ready\r\n")
            while line := self.rfile.readline():
                tag, _, rest = line.decode().strip().partition(" ")
                cmd = rest.upper()
                if cmd.startswith("CAPABILITY"):
                    self.send(f"* CAPABILITY IMAP4rev1 ID\r\n{tag} OK done\r\n")
                elif cmd.startswith("LOGIN"):
                    ok = '"secret-code"' in rest or " secret-code" in rest
                    self.send(f"{tag} OK logged in\r\n" if ok else f"{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n")
                elif cmd.startswith("ID"):
                    self.send(f'* ID ("name" "fake")\r\n{tag} OK done\r\n')
                elif cmd.startswith(("EXAMINE", "SELECT")):
                    n = len(MAILS)
                    self.send(f"* {n} EXISTS\r\n* OK [UIDVALIDITY 7] ok\r\n* OK [UIDNEXT {n + 1}] ok\r\n{tag} OK [READ-ONLY] done\r\n")
                elif cmd.startswith("UID SEARCH"):
                    lo = int(rest.split("UID ")[-1].split(":")[0])
                    uids = " ".join(str(u) for u in range(lo, len(MAILS) + 1))
                    self.send(f"* SEARCH {uids}\r\n{tag} OK done\r\n")
                elif cmd.startswith("UID FETCH"):
                    uid = int(rest.split()[2])
                    raw = MAILS[uid - 1]
                    self.send(f"* {uid} FETCH (UID {uid} RFC822.SIZE {len(raw)} BODY[]<0> {{{len(raw)}}}\r\n".encode() + raw + b")\r\n")
                    self.send(f"{tag} OK done\r\n")
                elif cmd.startswith("LOGOUT"):
                    self.send(f"* BYE bye\r\n{tag} OK done\r\n")
                    return
                else:
                    self.send(f"{tag} BAD unknown\r\n")

    srv = socketserver.ThreadingTCPServer(("127.0.0.1", 0), H)
    srv.daemon_threads = True
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


@pytest.fixture(scope="module")
def env(llm):
    import conftest
    tmp = Path(tempfile.mkdtemp(prefix="osa-sources-"))
    ca, crt, key = _certs(tmp)
    site, imap = _site(), _imap(crt, key)
    s = conftest.Server(llm, config={"tools": {"fetch_private": True}}, env={"SSL_CERT_FILE": str(ca)})
    yield s, f"http://127.0.0.1:{site.server_address[1]}", imap.server_address[1], tmp
    s.stop()
    site.shutdown()
    imap.shutdown()


def _routine(srv, bot, source: str, params: dict) -> dict:
    return srv.post(f"/api/bots/{bot['id']}/routines", {"name": source, "prompt": "看一下",
                                                         "trigger": {"kind": "event", "source": source, "params": params}})


def _mail(uid: int, subject: str) -> bytes:
    return (f"From: 张三 <zhang@example.com>\r\nTo: me@example.com\r\nSubject: {subject}\r\n"
            f"Date: Thu, 19 Nov 2026 10:0{uid}:00 +0800\r\nMessage-ID: <m{uid}@x>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
            f"第 {uid} 封信的正文\r\n").encode()


def test_mail_imap(env, fake):
    srv, _, port, _ = env
    MAILS[:] = [_mail(1, "旧信")]
    b = srv.make_bot("Mailer")
    bad = srv.http.post(f"/api/bots/{b['id']}/routines", json={"name": "x", "prompt": "x", "trigger": {"kind": "event", "source": "mail",
                        "params": {"provider": "custom", "user": "me", "password": "x", "host": "127.0.0.1", "port": port}}})
    assert bad.status_code == 400 and bad.json()["detail"] == "邮箱地址要写完整，比如 you@qq.com"
    r = _routine(srv, b, "mail", {"provider": "custom", "user": "me@example.com", "password": "secret-code", "host": "127.0.0.1", "port": port})
    assert r["describe"].startswith("收到新邮件：me@example.com（每 5 分钟")
    out = srv.post(f"/api/routines/{r['id']}/check")
    assert out["note"] == "已记下现状，之后有新的才通知", out
    assert srv.post(f"/api/routines/{r['id']}/check")["note"] == "没有新的"
    MAILS.append(_mail(2, "新的报价单"))
    fake.script("worker", "收到了")
    assert srv.post(f"/api/routines/{r['id']}/check")["note"] == "有新的，已经交给 Bot"
    until(lambda: srv.find(b["chat_id"], kind="result"), what="结果")
    goal = fake.bodies("worker")[0]["messages"][1]["content"]
    assert "张三 来信：新的报价单" in goal and "发件人：张三 <zhang@example.com>" in goal and "第 2 封信的正文" in goal, goal
    # 授权码错了：照 Python 版的说法报
    srv.put(f"/api/routines/{r['id']}", {"trigger": {"kind": "event", "source": "mail", "params": {
        "provider": "custom", "user": "me@example.com", "password": "wrong", "host": "127.0.0.1", "port": port}}})
    out = srv.post(f"/api/routines/{r['id']}/check")
    assert out["event"]["error"].startswith("登录失败：检查邮箱和授权码"), out
    assert "（服务器说：[AUTHENTICATIONFAILED] Invalid credentials）" in out["event"]["error"]
    idle(srv)


def _ics(now: float) -> bytes:
    z = lambda t: time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(t))   # noqa: E731
    soon, start = now + 10 * 60, now - 3 * 86400 + 20 * 60
    return ("BEGIN:VCALENDAR\r\nX-WR-TIMEZONE:Asia/Shanghai\r\n"
            f"BEGIN:VEVENT\r\nUID:meet\r\nDTSTART:{z(soon)}\r\nDTEND:{z(soon + 3600)}\r\nSUMMARY:周会\\, 第一次\r\nLOCATION:3 楼\r\n"
            "ORGANIZER;CN=Alice:mailto:alice@x.com\r\nEND:VEVENT\r\n"
            f"BEGIN:VEVENT\r\nUID:daily\r\nDTSTART:{z(start)}\r\nDURATION:PT15M\r\nRRULE:FREQ=DAILY;INTERVAL=1\r\n"
            f"EXDATE:{z(start + 86400)}\r\nSUMMARY:站会\r\nEND:VEVENT\r\n"
            f"BEGIN:VEVENT\r\nUID:gone\r\nDTSTART:{z(soon)}\r\nSTATUS:CANCELLED\r\nSUMMARY:取消了\r\nEND:VEVENT\r\n"
            "END:VCALENDAR\r\n").encode()


def test_calendar_ics(env, fake):
    srv, base, _, _ = env
    PAGES["/cal.ics"] = ("text/calendar", _ics(time.time()))
    PAGES["/notcal"] = ("text/html", b"<html>hi</html>")
    b = srv.make_bot("Planner")
    bad = srv.http.post(f"/api/bots/{b['id']}/routines", json={"name": "x", "prompt": "x", "trigger": {"kind": "event", "source": "calendar",
                        "params": {"url": "ftp://x"}}})
    assert bad.status_code == 400 and bad.json()["detail"] == "日历地址要以 https://、http:// 或 webcal:// 开头"
    r = _routine(srv, b, "calendar", {"url": base + "/cal.ics", "minutes_before": 30})
    assert r["describe"].startswith("日程快开始了：提前 30 分钟")
    fake.script("worker", "知道了")
    out = srv.post(f"/api/routines/{r['id']}/check")
    assert out["note"] == "有新的，已经交给 Bot", out   # 日历第一次看到就报（fire_first）
    until(lambda: srv.find(b["chat_id"], kind="result"), what="结果")
    goal = fake.bodies("worker")[0]["messages"][1]["content"]
    assert "「周会, 第一次」10 分钟后开始" in goal and "「站会」20 分钟后开始" in goal and "取消了" not in goal, goal
    assert "地点：3 楼" in goal and "组织者：Alice <alice@x.com>" in goal
    assert srv.post(f"/api/routines/{r['id']}/check")["note"] == "没有新的"
    r2 = _routine(srv, b, "calendar", {"url": base + "/notcal"})
    out = srv.post(f"/api/routines/{r2['id']}/check")
    assert out["event"]["error"].startswith("这个地址返回的不是日历（ICS）"), out
    idle(srv)


def test_folder(env, fake):
    srv, _, _, tmp = env
    d = tmp / "inbox"
    d.mkdir()
    (d / "old.pdf").write_bytes(b"x" * 5)
    (d / ".hidden.pdf").write_bytes(b"x")
    b = srv.make_bot("Filer")
    bad = srv.http.post(f"/api/bots/{b['id']}/routines", json={"name": "x", "prompt": "x", "trigger": {"kind": "event", "source": "folder",
                        "params": {"path": str(tmp / "nope")}}})
    assert bad.status_code == 400 and bad.json()["detail"].startswith("文件夹不存在：")
    r = _routine(srv, b, "folder", {"path": str(d), "pattern": "*.pdf, *.png"})
    assert r["describe"].startswith("文件夹有新文件：") and "（*.pdf, *.png）" in r["describe"]
    assert srv.post(f"/api/routines/{r['id']}/check")["note"] == "已记下现状，之后有新的才通知"
    (d / "发票.PDF").write_bytes(b"y" * 2048)
    (d / "notes.txt").write_bytes(b"z")
    assert srv.post(f"/api/routines/{r['id']}/check")["note"] == "没有新的"   # 先等一次，确认写完了
    fake.script("worker", "新发票")
    assert srv.post(f"/api/routines/{r['id']}/check")["note"] == "有新的，已经交给 Bot"
    until(lambda: srv.find(b["chat_id"], kind="result"), what="结果")
    goal = fake.bodies("worker")[0]["messages"][1]["content"]
    assert "新文件：发票.PDF" in goal and "大小：2.0 KB" in goal and "notes.txt" not in goal and "old.pdf" not in goal, goal
    idle(srv)
