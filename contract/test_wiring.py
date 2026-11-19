"""模块之间接起来的地方：模板带的定时任务装上、导出时去掉密钥；设置里选画图 / 视频模型要是对的模型；
头像用画图模型画（假的画图服务）；限时的媒体地址；视频和分段不压缩；Bot 删掉时定时任务跟着删。"""
import base64
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from conftest import IMPL, Server

PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")


class Vendor:
    """假的画图服务（OpenAI 那种 /v1/images/generations）。"""

    def __init__(self):
        self.prompts: list[str] = []
        fake = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _send(self, code, body: bytes):
                self.send_response(code)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                if self.path.endswith("/models"):
                    self._send(200, json.dumps({"object": "list", "data": [{"id": "fake-draw"}]}).encode())
                else:
                    self._send(404, b"{}")

            def do_POST(self):
                n = int(self.headers.get("content-length") or 0)
                body = json.loads(self.rfile.read(n) or b"{}")
                if self.path.endswith("/images/generations"):
                    fake.prompts.append(body.get("prompt", ""))
                    self._send(200, json.dumps({"data": [{"b64_json": base64.b64encode(PNG).decode()}]}).encode())
                else:
                    self._send(404, b"{}")

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()


@pytest.fixture(scope="module")
def vendor():
    return Vendor()


@pytest.fixture(scope="module")
def srv(llm, vendor):
    config = {
        "providers": {"fake": {"base_url": llm.url, "api_key_env": "FAKE_LLM_KEY"},
                      "fakeimg": {"base_url": vendor.url + "/v1"}},
        "generation": {"recipes": [{"name": "fakeimg", "kind": "image", "host": "fakeimg", "model": "^fake-draw$",
                                    "request": {"path": "/images/generations", "json": {"model": "$model", "prompt": "$prompt", "n": 1}},
                                    "result": ["data.0.url", "data.0.b64_json"]}]},
    }
    s = Server(llm, config=config)
    yield s
    s.stop()


TEMPLATE = """osa_template: 1
name: 盯仓库
instructions: 你负责盯着 GitHub 仓库
routines:
  - name: 每天早报
    prompt: 整理一下昨天的动态
    trigger: {kind: daily, times: ["08:30"], days: []}
  - name: 收到推送
    prompt: "看看推送了什么：{event}"
    trigger: {kind: event, source: webhook, params: {}, key: will-be-replaced-0000}
"""


def test_template_installs_routines_and_export_strips_keys(srv):
    p = srv.post("/api/templates/preview", {"text": TEMPLATE})
    assert [r["name"] for r in p["routines"]] == ["每天早报", "收到推送"]
    assert all(r["when"] for r in p["routines"])   # routines.describe 写了什么时候跑
    b = srv.post("/api/templates/install", {"text": TEMPLATE, "enable_routines": False})
    rs = srv.get(f"/api/bots/{b['id']}/routines")
    assert [r["name"] for r in rs] == ["每天早报", "收到推送"]
    assert all(r["enabled"] is False for r in rs)
    ev = next(r for r in rs if r["name"] == "收到推送")
    assert ev["trigger"]["kind"] == "event" and ev["trigger"]["key"] != "will-be-replaced-0000"
    out = srv.http.get(f"/api/bots/{b['id']}/template").text
    assert "每天早报" in out and ev["trigger"]["key"] not in out
    # 删 Bot：它的定时任务和事件一起删
    srv.delete(f"/api/bots/{b['id']}")
    assert not [r for r in srv.get("/api/routines") if r["bot_id"] == b["id"]]


def test_settings_checks_image_and_video_models(srv):
    r = srv.http.put("/api/settings", json={"image_model": {"provider": "fake", "model": "fake-model"}})
    assert r.status_code == 400 and r.json()["detail"] == "fake-model 不是画图的模型"
    r = srv.http.put("/api/settings", json={"image_edit_model": {"provider": "fakeimg", "model": "fake-draw"}})
    assert r.status_code == 400 and r.json()["detail"] == "fake-draw 不能照着图片画"
    r = srv.http.put("/api/settings", json={"video_model": {"provider": "fakeimg", "model": "fake-draw"}})
    assert r.status_code == 400 and r.json()["detail"] == "fake-draw 不是生成视频的模型"
    s = srv.put("/api/settings", {"image_model": {"provider": "fakeimg", "model": "fake-draw"}})
    assert s["image_model"] == {"provider": "fakeimg", "model": "fake-draw"}
    s = srv.put("/api/settings", {"image_model": {"provider": "", "model": ""}, "video_model": {"provider": "", "model": ""}})
    assert s["image_model"] == {"provider": "", "model": ""} and s["video_model"] == {"provider": "", "model": ""}


def test_avatar_models_and_generate(srv, vendor):
    m = srv.get("/api/avatars/models")
    assert [(x["provider"], x["model"]) for x in m["models"]] == [("fakeimg", "fake-draw")]
    assert m["default"]["model"] == "fake-draw" and m["edit_models"] == [] and m["edit_default"] is None
    r = srv.http.post("/api/avatars/generate", json={"prompt": "  "})
    assert r.status_code == 400
    g = srv.post("/api/avatars/generate", {"prompt": "一只戴眼镜的猫", "provider": "fakeimg", "model": "fake-draw"})
    assert g["data"].startswith("data:image/png;base64,") and base64.b64decode(g["data"].split(",", 1)[1]) == PNG
    assert vendor.prompts[-1] == "一只戴眼镜的猫"
    assert srv.get("/api/settings")["image_model"] == {"provider": "fakeimg", "model": "fake-draw"}   # 画过就记下


def test_video_and_ranges_not_compressed(srv):
    video = b"\x00\x00\x00\x18ftypmp42" + bytes(range(256)) * 40
    mid = "cd" * 12
    import sqlite3
    c = sqlite3.connect(srv.data / "osa.db")
    c.execute("insert or replace into media(id, mime, name, size, w, h, data, created) values(?,?,?,?,0,0,?,?)",
              (mid, "video/mp4", "v.mp4", len(video), video, time.time()))
    c.commit()
    c.close()
    full = srv.http.get(f"/api/media/{mid}", headers={"accept-encoding": "gzip, br"})
    assert full.status_code == 200 and full.content == video
    assert "content-encoding" not in full.headers or full.headers["content-encoding"] == "identity"
    assert full.headers["accept-ranges"] == "bytes"
    part = srv.http.get(f"/api/media/{mid}", headers={"Range": "bytes=10-19", "accept-encoding": "gzip, br"})
    assert part.status_code == 206 and part.content == video[10:20]
    if IMPL == "rust":   # Rust 版顺手加的：接口的 JSON 照样压缩（Python 版不压）
        big = srv.http.get("/api/bootstrap", headers={"accept-encoding": "gzip"})
        assert big.headers.get("content-encoding") == "gzip"


def test_pub_media_link_signature(srv):
    mid = "cd" * 12
    r = srv.http.get(f"/pub/media/{mid}", params={"exp": str(int(time.time()) + 600), "sig": "0" * 32})
    assert r.status_code in (403, 404)
    r = srv.http.get(f"/pub/media/{mid}", params={"exp": str(int(time.time()) - 10), "sig": "0" * 32})
    assert r.status_code in (403, 404)
