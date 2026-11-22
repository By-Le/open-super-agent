"""设置、模型服务、单价、用量、观测的接口。"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
from conftest import Server, Socket, idle, until
from fakellm_http import Call

SETTINGS_KEYS = {"providers", "presets", "roles", "limits", "heavy_workers", "computer_parallel", "computer_backend",
                 "computer_backends", "computer_current", "computer_choices", "computer_on_desktop", "computer_host_shell",
                 "embedding", "image_model", "image_edit_model", "video_model", "model_info", "prices", "problem",
                 "language", "security", "docs"}

# 冒充 models.dev：一家 DeepSeek（地址和 api.deepseek.com 对上），一家没地址的
MODELS_DEV = {
    "deepseek": {"name": "DeepSeek", "api": "https://api.deepseek.com", "models": {
        "deepseek-x": {"name": "DeepSeek X", "cost": {"input": 1.0, "output": 2.0, "cache_read": 0.1},
                       "modalities": {"input": ["text", "image"]}},
        "deepseek-emb": {"name": "Emb", "type": "embedding", "cost": {}},
        "no-price": {"name": "No price", "cost": {}}}},
    "other": {"name": "Other", "models": {"m1": {"cost": {"input": 3, "output": 4}}}},
}


@pytest.fixture(scope="module")
def dev():
    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            body = json.dumps(MODELS_DEV).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{httpd.server_address[1]}/api.json"
    httpd.shutdown()


@pytest.fixture(scope="module")
def srv(llm, dev):
    import yaml
    from conftest import ROOT
    pricing = yaml.safe_load((ROOT / "config.yaml").read_text())["pricing"]
    s = Server(llm, config={"pricing": {**pricing, "source": dev}})
    yield s
    s.stop()


def test_settings_shape(srv):
    s = srv.get("/api/settings")
    assert set(s) == SETTINGS_KEYS
    assert s["roles"]["router"] == {"provider": "fake", "model": "fake-model"}
    assert set(s["limits"]) == {"daily_token_budget", "worker_max_steps", "task_timeout_s", "approval_timeout_s"}
    assert s["problem"] == ""
    assert s["language"] in ("zh", "en")
    assert s["model_info"]["fake/fake-model"] == {"source": "local", "input": 0, "output": 0, "cache": 0}
    assert [p["name"] for p in s["providers"]] == ["fake"]
    assert s["providers"][0]["needs_key"] is False and s["providers"][0]["source"] == "config"
    assert [x["id"] for x in s["computer_backends"]] == ["host", "local", "instabox"]
    assert s["embedding"] == {"provider": "", "model": ""}
    assert s["image_model"] == {"provider": "", "model": ""}
    assert s["security"]["local_exec"] is True
    assert {"source", "updated", "error", "rate", "currency", "models"} <= set(s["prices"])
    assert any(p["id"] == "deepseek" for p in s["presets"])


def test_settings_save(srv):
    s = srv.put("/api/settings", {"limits": {"worker_max_steps": "12", "nope": 3}, "heavy_workers": 20})
    assert s["limits"]["worker_max_steps"] == 12 and s["heavy_workers"] == 8
    s = srv.put("/api/settings", {"heavy_workers": 0, "roles": {"worker": {"model": "  other-model  ", "x": 1}, "ghost": {"model": "y"}}})
    assert s["heavy_workers"] == 1
    assert s["roles"]["worker"] == {"provider": "fake", "model": "other-model"}
    assert "ghost" not in s["roles"]
    assert "fake/other-model" in s["model_info"]
    for body, word in [({"roles": {"worker": {"model": " "}}}, "模型名字不能为空"),
                       ({"roles": {"worker": {"provider": "nope"}}}, "nope"),
                       ({"embedding": {"provider": "fake"}}, "embedding"),
                       ({"computer_backend": "moon"}, "moon"),
                       ({"heavy_workers": "many"}, "invalid literal"),
                       ({"language": "fr"}, "fr")]:
        r = srv.http.put("/api/settings", json=body)
        assert r.status_code == 400, body
        assert word in r.json()["detail"], (body, r.json())
    s = srv.put("/api/settings", {"embedding": {"provider": "fake", "model": "emb-1"},
                                  "video_model": None, "image_model": {"provider": "", "model": ""}, "roles": {"worker": {"model": "fake-model"}}})
    assert s["embedding"] == {"provider": "fake", "model": "emb-1"}
    assert s["image_model"] == {"provider": "", "model": ""}
    assert s["video_model"] == {"provider": "", "model": ""}
    assert "fake/emb-1" in s["model_info"]
    s = srv.put("/api/settings", {"embedding": None, "computer_backend": None})
    assert s["embedding"] == {"provider": "", "model": ""} and s["computer_backend"] == "local"
    # 换语言：报错跟着变成英文
    srv.put("/api/settings", {"language": "en"})
    r = srv.http.put("/api/settings", json={"roles": {"worker": {"model": ""}}})
    assert r.status_code == 400 and r.json()["detail"] == "Model name can't be empty"
    assert srv.put("/api/settings", {"language": "zh"})["language"] == "zh"
    # 存进去了：重新读一样
    assert srv.get("/api/settings")["limits"]["worker_max_steps"] == 12
    with srv.db() as c:
        o = json.loads(c.execute("select value from settings where key='overrides'").fetchone()[0])
    assert o["limits"]["worker_max_steps"] == 12 and o["heavy_workers"] == 1


def test_providers(srv, fake):
    url = srv.get("/api/settings")["providers"][0]["base_url"]
    r = srv.post("/api/providers/check", {"base_url": url + "/"})
    assert r == {"ok": True, "models": ["fake"], "error": "", "note": ""}
    assert srv.http.post("/api/providers/check", json={"base_url": "ftp://x"}).status_code == 400
    # 新建一个（本机地址不用密钥）
    r = srv.post("/api/providers", {"title": "My Box", "base_url": url, "preset": "custom"})
    name = r["name"]
    assert name == "mybox"
    p = next(x for x in r["settings"]["providers"] if x["name"] == name)
    assert p["title"] == "My Box" and p["source"] == "ui" and p["preset"] == "custom" and p["needs_key"] is False
    m = srv.get(f"/api/providers/{name}/models")
    assert m["ok"] is True and m["models"] == ["fake"] and m["name"] == name
    assert m["items"] == [{"id": "fake", "kind": "chat", "input": 0, "output": 0}]
    assert srv.get(f"/api/providers/{name}/models?refresh=true")["models"] == ["fake"]
    assert srv.http.get("/api/providers/ghost/models").status_code == 400
    r = srv.put(f"/api/providers/{name}", {"title": "Box 2", "base_url": url})
    assert next(x for x in r["settings"]["providers"] if x["name"] == name)["title"] == "Box 2"
    # 第二个同名的加后缀
    assert srv.post("/api/providers", {"title": "My Box", "base_url": url, "preset": "custom"})["name"] == "mybox_2"
    # 在用的、config.yaml 里的不能删
    srv.put("/api/settings", {"roles": {"filter": {"provider": name, "model": "fake"}}})
    r = srv.http.delete(f"/api/providers/{name}")
    assert r.status_code == 400 and "还在用它" in r.json()["detail"]
    assert srv.http.delete("/api/providers/fake").status_code == 400
    srv.put("/api/settings", {"roles": {"filter": {"provider": "fake", "model": "fake-model"}}})
    names = [p["name"] for p in srv.delete(f"/api/providers/{name}")["settings"]["providers"]]
    assert name not in names and "mybox_2" in names
    srv.delete("/api/providers/mybox_2")


def test_model_test(srv, fake):
    r = srv.post("/api/models/test", {"provider": "fake", "model": "fake-model"})
    assert r["ok"] is True and r["reply"] == "完成" and r["vision"] == "unsure" and isinstance(r["ms"], int)
    fake.script("other", "红色")
    r = srv.post("/api/models/test", {"provider": "fake", "model": "seeing"})
    assert r["vision"] == "yes"
    assert srv.get("/api/settings")["model_info"]["fake/fake-model"]["source"] == "local"
    assert srv.http.post("/api/models/test", json={"provider": "fake", "model": ""}).status_code == 400
    assert srv.http.post("/api/models/test", json={"provider": "ghost", "model": "x"}).status_code == 400


def test_prices(srv):
    i = srv.get("/api/prices")
    assert {"source", "updated", "error", "rate", "currency", "models"} == set(i)
    assert i["rate"] == 7.1 and i["currency"] == "¥"
    r = srv.post("/api/prices/refresh")
    assert r["error"] == "" and r["models"] == 3 and r["updated"] > 0 and set(r["settings"]) == SETTINGS_KEYS
    # 一个地址和 models.dev 对上的服务（不调它，只看单价）
    name = srv.post("/api/providers", {"preset": "deepseek", "base_url": "https://api.deepseek.com"})["name"]
    assert name == "deepseek"
    s = srv.put("/api/settings", {"roles": {"reviewer": {"provider": name, "model": "deepseek-x"}}})
    assert s["model_info"]["deepseek/deepseek-x"] == {"source": "models.dev", "vision": True, "kind": "chat", "name": "DeepSeek X",
                                                      "input": 7.1, "output": 14.2, "cache": 0.71, "via": ""}
    s = srv.put("/api/settings", {"roles": {"reviewer": {"model": "deepseek-flash"}}})   # config.yaml 里写了两档价
    info = s["model_info"]["deepseek/deepseek-flash"]
    assert info["source"] == "config" and info["input"] == 2 and info["offpeak"] == [1, 4]
    s = srv.put("/api/settings", {"roles": {"reviewer": {"model": "deepseek-emb"}}})
    assert s["model_info"]["deepseek/deepseek-emb"] == {"source": "", "vision": False, "kind": "embedding", "name": "Emb"}
    assert s["problem"] == ""
    srv.put("/api/settings", {"roles": {"reviewer": {"provider": "fake", "model": "fake-model"}}})
    srv.delete(f"/api/providers/{name}")


def test_usage_and_observe(srv, fake):
    ws = Socket(srv)
    try:
        a = srv.make_bot("Counter")
        fake.script("router:Counter", Call("dispatch", {"goal": "数一数"}), "派了")
        fake.script("worker", [Call("ghost_tool", {})], "数完了")
        ws.say(a["chat_id"], "数一数")
        until(lambda: srv.find(a["chat_id"], kind="result"), what="任务结果")
        idle(srv)
        fake.fail(500, 500, 500, 500, 500, 500)
        ws.say(a["chat_id"], "再来")
        until(lambda: ws.of("error"), timeout=60, what="出错")
    finally:
        ws.close()
    u = srv.get("/api/usage")
    assert {"currency", "today", "week", "month", "by_role", "by_task", "by_day"} == set(u)
    assert u["today"]["calls"] >= 3
    o = srv.get("/api/observe?range=today&tz=-480")
    assert {"currency", "range", "start", "end", "size", "kpi", "prev", "series", "bots", "models", "roles", "tools",
            "routines", "tasks", "problems", "hours"} == set(o)
    assert o["range"] == "today" and o["size"] == 3600 and len(o["series"]) == 24 and len(o["hours"]) == 7
    k = o["kpi"]
    assert k["tasks"] >= 1 and k["done"] >= 1 and k["errors"] >= 1 and k["tool_calls"] >= 1 and k["tool_errors"] >= 1
    assert sum(x["tasks"] for x in o["series"]) == k["tasks"]
    assert any(b["bot_id"] == a["id"] and b["tasks"] == 1 and b["done"] == 1 for b in o["bots"])
    assert any(t["goal"] == "数一数" and t["status"] == "done" for t in o["tasks"])
    assert {p["kind"] for p in o["problems"]} >= {"model", "tool"}
    assert any(t["name"] == "ghost_tool" and t["errors"] == 1 for t in o["tools"])
    assert any(m["provider"] == "fake" and m["model"] == "fake-model" for m in o["models"])
    assert srv.get("/api/observe?range=bogus")["range"] == "7d"
    assert srv.get("/api/observe?range=all")["prev"] is None
    assert srv.get(f"/api/observe?bot={a['id']}")["kpi"]["tasks"] == 1
    c = srv.get("/api/observe/calls?range=today&limit=2")
    assert len(c["items"]) == 2 and c["more"] is True and c["total"] >= 3
    assert list(c["items"][0]) == ["id", "ts", "bot_id", "chat_id", "task_id", "role", "provider", "model", "input",
                                   "cache_hit", "output", "cost", "ms", "ttft", "err"]
    nxt = srv.get(f"/api/observe/calls?range=today&limit=2&before={c['items'][-1]['id']}")
    assert nxt["items"][0]["id"] < c["items"][-1]["id"]
    bad = srv.get("/api/observe/calls?range=today&failed=true")
    assert bad["items"] and all(x["err"] for x in bad["items"])
    r = srv.http.get("/api/observe/calls.csv?range=today")
    assert r.status_code == 200 and r.headers["content-type"].startswith("text/csv")
    assert "attachment" in r.headers["content-disposition"]
    lines = r.text.splitlines()
    assert lines[0] == "﻿time,bot,bot_id,chat_id,task_id,role,provider,model,input,cache_hit,output,cost,ms,ttft,err"
    assert len(lines) == c["total"] + 1 and any(",Counter," in ln for ln in lines)
    st = srv.get("/api/status")
    assert {"mcp", "mcp_tools", "mcp_confirm", "routines"} == set(st)


def test_onboarding(srv):
    assert srv.post("/api/onboarding/done") == {"ok": True}
    assert srv.get("/api/bootstrap")["onboarding"] is False
