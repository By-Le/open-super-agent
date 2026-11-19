"""搜索网页、读网页、画图：fetch_url 挡内网和不用的网站，web_search 按配方调一家假的搜索服务，
generate_image 按配方调一家假的画图服务（本机地址，不用密钥、价格 0），先弹确认卡片。不连外网，不调收钱的接口。"""
import base64
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

import pytest

from conftest import Server, idle, until
from fakellm_http import Call

PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")


class Vendor:
    """假的搜索服务 + 假的画图服务（OpenAI 那种 /v1/images/generations）。"""

    def __init__(self):
        self.requests: list[tuple[str, str, dict, bytes]] = []
        fake = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _send(self, code, body: bytes, ctype="application/json"):
                self.send_response(code)
                self.send_header("content-type", ctype)
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def _handle(self, method):
                n = int(self.headers.get("content-length") or 0)
                body = self.rfile.read(n) if n else b""
                u = urlsplit(self.path)
                q = {k: v[0] for k, v in parse_qs(u.query).items()}
                fake.requests.append((method, u.path, q, body))
                if u.path == "/search":
                    items = [{"name": "Rust <b>语言</b>", "link": "https://www.rust-lang.org/", "snippet": "安全 &amp; 快", "published": "2026-11-01T10:00:00Z"},
                             {"name": "被挡的", "link": "https://sub.blocked.invalid/page", "desc": "不该出现"},
                             {"name": "没有链接"},
                             {"name": "", "link": "https://docs.rs/", "desc": "  多  空格 "}]
                    if q.get("q") == "空":
                        items = []
                    self._send(200, json.dumps({"data": {"items": items}}).encode())
                elif u.path == "/v1/models":
                    self._send(200, json.dumps({"object": "list", "data": [{"id": "fake-draw"}]}).encode())
                elif u.path == "/v1/images/generations":
                    self._send(200, json.dumps({"data": [{"b64_json": base64.b64encode(PNG).decode()}]}).encode())
                else:
                    self._send(404, b'{"error":{"message":"nope"}}')

            def do_GET(self):
                self._handle("GET")

            def do_POST(self):
                self._handle("POST")

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.port = self.httpd.server_address[1]
        self.url = f"http://127.0.0.1:{self.port}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()


@pytest.fixture(scope="module")
def vendor():
    return Vendor()


@pytest.fixture(scope="module")
def srv(llm, vendor):
    recipe = {"name": "fakesearch", "kind": "search", "title": "假搜索",
              "request": {"method": "GET", "url": vendor.url + "/search", "params": {"q": "$query", "n": "$count"}},
              "recent": {"params": {"q": "$query", "n": "$count", "since": "$since", "range": "$range"}},
              "ranges": {"week": "w7"},
              "results": ["nothing.here", "data.items"],
              "item": {"title": "name", "url": "link", "text": ["snippet", "desc"], "date": "published"}}
    config = {
        "providers": {"fake": {"base_url": llm.url, "api_key_env": "FAKE_LLM_KEY"},
                      "fakeimg": {"base_url": vendor.url + "/v1"}},
        "web": {"search": ["fakesearch"], "fetch": ["direct"], "block": ["blocked.invalid"], "recipes": [recipe]},
        "generation": {"recipes": [{"name": "fakeimg", "kind": "image", "host": "fakeimg", "model": "^fake-draw$",
                                    "request": {"path": "/images/generations", "json": {"model": "$model", "prompt": "$prompt", "n": 1}},
                                    "result": ["data.0.url", "data.0.b64_json"]}]},
    }
    s = Server(llm, config=config)
    yield s
    s.stop()


def tool_outputs(fake) -> list[str]:
    """最后一次请求里的工具结果（每次请求都带着前面的全部）。"""
    return [m["content"] for m in fake.bodies("worker")[-1]["messages"] if m["role"] == "tool"]


def test_web_settings_shape(srv):
    w = srv.get("/api/web")
    assert w["block"] == ["blocked.invalid"]
    assert set(w) >= {"block", "default_block", "posts_budget_usd", "usd_rate", "search", "fetch", "posts"}
    names = [s["name"] for s in w["search"]["services"]]
    assert names[0] == "fakesearch" and "duckduckgo" in names
    assert w["search"]["auto"] is False
    d = next(s for s in w["search"]["services"] if s["name"] == "duckduckgo")
    assert d["missing"] == "" and d["price"] == 0
    r = srv.http.put("/api/web", json={"block": ["not a domain"]})
    assert r.status_code == 400 and "不是域名" in r.json()["detail"]


def test_fetch_guard(srv, fake, ws):
    a = srv.make_bot("Fetcher")
    fake.script("router:Fetcher", Call("dispatch", {"goal": "读几个网页"}), "去读了")
    fake.script("worker", [Call("fetch_url", {"url": "http://127.0.0.1:9/secret"}),
                           Call("fetch_url", {"url": "https://sub.blocked.invalid/x"}),
                           Call("fetch_url", {"url": "ftp://example.com/"})], "读完了")
    ws.say(a["chat_id"], "读网页")
    until(lambda: srv.find(a["chat_id"], kind="result"), what="任务结果")
    out = tool_outputs(fake)
    assert out[0] == "没抓取：127.0.0.1 是本机或内网地址。要抓内网页面，在 config.yaml 里设 tools.fetch_private: true"
    assert out[1] == "没抓取：sub.blocked.invalid 在不用的网站里（设置 → 搜索和读网页），不要从这个网站找信息"
    assert out[2] == "只能抓取 http / https 网址"
    idle(srv)


def test_search_recipe(srv, fake, ws, vendor):
    a = srv.make_bot("Searcher")
    fake.script("router:Searcher", Call("dispatch", {"goal": "搜一下"}), "去搜了")
    fake.script("worker", Call("web_search", {"query": "rust"}), Call("web_search", {"query": "rust", "days": 7}),
                Call("web_search", {"query": "空"}), "搜完了")
    vendor.requests.clear()
    ws.say(a["chat_id"], "搜 rust")
    until(lambda: srv.find(a["chat_id"], kind="result"), what="任务结果")
    out = tool_outputs(fake)
    assert out[0] == ("- Rust 语言 | https://www.rust-lang.org/ | 2026-11-01\n  安全 & 快\n"
                      "- https://docs.rs/ | https://docs.rs/\n  多 空格")
    assert out[2].startswith("无结果\n（假搜索：没有结果）")
    reqs = [q for m, p, q, _ in vendor.requests if p == "/search"]
    assert reqs[0] == {"q": "rust", "n": "8"}
    assert reqs[1]["range"] == "w7" and len(reqs[1]["since"]) == 10
    idle(srv)


def test_generate_image_with_approval(srv, fake, ws, vendor):
    a = srv.make_bot("Painter")
    fake.script("router:Painter", Call("generate_image", {"prompt": "一只橘猫"}), "确认一下")
    vendor.requests.clear()
    ws.say(a["chat_id"], "画一只橘猫")
    card = until(lambda: [e for e in ws.of("approval") if e.get("status") == "pending" and e.get("tool") == "generate_image"],
                 what="画图的确认卡片")[0]
    assert "一只橘猫" in card["args"] and "fake-draw" in card["args"] and "免费" in card["args"]
    assert card["scopes"] == ["once", "always"]
    tool_out = [m["content"] for b in fake.bodies("router", "Painter") for m in b["messages"] if m["role"] == "tool"]
    assert tool_out[0].startswith("已弹出确认卡片（模型 fake-draw，免费）")
    ws.send({"type": "approve", "id": card["id"], "ok": True})
    img = until(lambda: srv.find(a["chat_id"], kind="image"), what="画好的图")[0]
    assert img["content"] == "[画图] 一只橘猫"
    assert img["meta"]["model"] == "fake-draw" and img["meta"]["provider"] == "fakeimg" and img["meta"]["price"] == 0
    att = img["meta"]["attachments"][0]
    assert att["mime"] == "image/png" and att["w"] == 1 and att["h"] == 1
    body = json.loads(next(b for m, p, q, b in vendor.requests if p == "/v1/images/generations"))
    assert body == {"model": "fake-draw", "prompt": "一只橘猫", "n": 1}
    r = srv.http.get(f"/api/media/{att['media']}")
    assert r.status_code in (200, 501)   # /api/media 是资料库那边的
    assert srv.get("/api/videos/models") == {"models": [], "default": None}
