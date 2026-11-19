"""推送中转（osa.relay / osa-relay）：单独的进程。登记、改、删地址；推送的验签名、格式检查。
没配苹果推送密钥，签名对、格式对的推送会走到“转给苹果”这一步，回 502 apns key not configured。"""
import base64
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import httpx
import pytest
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature

from conftest import IMPL, ROOT

ORIGIN = "https://relay.test"
TOKEN = "ab" * 32


def b64u(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def point(k) -> bytes:
    return k.public_key().public_bytes(serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint)


def vapid(key, aud: str = ORIGIN, exp: float | None = None, sub: str = "mailto:t@example.com", alg: str = "ES256") -> str:
    head = b64u(json.dumps({"typ": "JWT", "alg": alg}).encode())
    claims = b64u(json.dumps({"aud": aud, "exp": int(exp or time.time() + 3600), "sub": sub}).encode())
    r, s = decode_dss_signature(key.sign(f"{head}.{claims}".encode(), ec.ECDSA(hashes.SHA256())))
    return f"vapid t={head}.{claims}.{b64u(r.to_bytes(32, 'big') + s.to_bytes(32, 'big'))}, k={b64u(point(key))}"


@pytest.fixture(scope="module")
def relay():
    tmp = Path(tempfile.mkdtemp(prefix=f"osa-relay-{IMPL}-"))
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    env = {k: v for k, v in os.environ.items() if not k.startswith("APNS_")}
    env |= {"OSA_RELAY_ORIGIN": ORIGIN, "OSA_RELAY_DB": str(tmp / "relay.db"), "NO_PROXY": "*", "no_proxy": "*"}
    if IMPL == "rust":
        cmd = [os.environ.get("OSA_RELAY_BIN", str(ROOT / "server" / "target" / "debug" / "osa-relay")), "--host", "127.0.0.1", "--port", str(port)]
    else:
        cmd = [sys.executable, "-m", "uvicorn", "osa.relay:app", "--host", "127.0.0.1", "--port", str(port), "--no-access-log"]
    log = open(tmp / "relay.log", "w")
    proc = subprocess.Popen(cmd, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT)
    c = httpx.Client(base_url=f"http://127.0.0.1:{port}", timeout=10, trust_env=False)
    end = time.time() + 20
    while True:
        try:
            if c.get("/healthz").status_code == 200:
                break
        except httpx.HTTPError:
            pass
        assert proc.poll() is None and time.time() < end, (tmp / "relay.log").read_text()
        time.sleep(0.05)
    yield c
    proc.terminate()
    proc.wait(10)
    log.close()


@pytest.fixture(scope="module")
def server_key():
    return ec.generate_private_key(ec.SECP256R1())


def _register(relay, key, token=TOKEN, env="production"):
    return relay.post("/v1/endpoints", json={"token": token, "env": env, "vapid": b64u(point(key))})


def test_health(relay):
    d = relay.get("/healthz").json()
    assert d["ok"] is True and d["apns"] is False


def test_register_update_remove(relay, server_key):
    assert _register(relay, server_key, token="xyz").status_code == 400
    assert _register(relay, server_key, env="dev").status_code == 400
    assert relay.post("/v1/endpoints", json={"token": TOKEN, "env": "production", "vapid": "bad"}).status_code == 400
    r = _register(relay, server_key)
    assert r.status_code == 201
    reg = r.json()
    assert reg["endpoint"] == f"{ORIGIN}/w/{reg['id']}" and reg["secret"]
    assert relay.put("/v1/endpoints/nope", json={}).status_code == 404
    assert relay.put(f"/v1/endpoints/{reg['id']}", json={"env": "sandbox"}, headers={"authorization": "Bearer wrong"}).status_code == 401
    ok = relay.put(f"/v1/endpoints/{reg['id']}", json={"env": "sandbox"}, headers={"authorization": f"Bearer {reg['secret']}"})
    assert ok.status_code == 200 and ok.json() == {"id": reg["id"], "endpoint": reg["endpoint"]}
    assert relay.put(f"/v1/endpoints/{reg['id']}", json={"env": "x"}, headers={"authorization": f"Bearer {reg['secret']}"}).status_code == 400
    assert relay.delete(f"/v1/endpoints/{reg['id']}", headers={"authorization": "Bearer wrong"}).status_code == 401
    assert relay.delete(f"/v1/endpoints/{reg['id']}", headers={"authorization": f"Bearer {reg['secret']}"}).status_code == 204
    assert relay.delete(f"/v1/endpoints/{reg['id']}").status_code == 204   # 已经没了


def test_push_checks(relay, server_key):
    reg = _register(relay, server_key).json()
    path = f"/w/{reg['id']}"
    body = os.urandom(16) + (4096).to_bytes(4, "big") + bytes([65]) + point(ec.generate_private_key(ec.SECP256R1())) + os.urandom(40)
    good = {"authorization": vapid(server_key), "content-encoding": "aes128gcm", "ttl": "60", "urgency": "high", "topic": "abc"}

    def push(headers=None, content=body, p=path):
        return relay.post(p, content=content, headers={**good, **(headers or {})})

    assert push(p="/w/nope").status_code == 404
    r = relay.post(path, content=body, headers={k: v for k, v in good.items() if k != "authorization"})
    assert r.status_code == 401 and r.json()["detail"] == "missing vapid authorization"
    assert push({"authorization": vapid(ec.generate_private_key(ec.SECP256R1()))}).json()["detail"] == "key does not match this endpoint"
    assert push({"authorization": vapid(server_key, aud="https://other")}).json()["detail"] == "wrong audience"
    assert push({"authorization": vapid(server_key, exp=time.time() - 10)}).status_code == 401
    assert push({"authorization": vapid(server_key, exp=time.time() + 3 * 86400)}).status_code == 401
    assert push({"authorization": vapid(server_key, sub="")}).json()["detail"] == "vapid token has no subject"
    bad_sig = good["authorization"].replace("vapid t=", "vapid t=x")
    assert push({"authorization": bad_sig}).status_code == 401
    assert push({"content-encoding": "aesgcm"}).status_code == 400
    assert push({"ttl": "-1"}).json()["detail"] == "missing or bad TTL"
    assert push(content=b"short").json()["detail"] == "not an aes128gcm message"
    assert push(content=body + os.urandom(3000)).status_code == 413
    r = push()
    assert r.status_code == 502 and r.json()["detail"] == "apns 502 apns key not configured"
