"""定时任务和事件：建、改、暂停、立即运行（派后台任务）、webhook 事件的关键词过滤、无新内容不打扰、事件来源列表。"""
import time

from conftest import idle, until
from fakellm_http import Call


def _runs(srv, rid):
    return srv.get(f"/api/routines/{rid}/runs")


def test_create_and_edit_schedule(srv, fake):
    b = srv.make_bot("Clock")
    r = srv.post(f"/api/bots/{b['id']}/routines", {"name": "早报", "prompt": "整理今天的新闻",
                                                   "trigger": {"kind": "daily", "times": ["9:00", "18:30"], "days": [1, 3, 5]}})
    for k in ("id", "bot_id", "name", "prompt", "skill", "trigger", "enabled", "next_run", "created", "updated", "running",
              "last_run", "category", "describe"):
        assert k in r, k
    assert r["trigger"] == {"kind": "daily", "times": ["09:00", "18:30"], "days": [1, 3, 5]}
    assert r["category"] == "schedule" and r["enabled"] is True and r["next_run"] > time.time()
    assert r["describe"] == "每周一、三、五 09:00、18:30"
    assert any(x["id"] == r["id"] for x in srv.get(f"/api/bots/{b['id']}/routines"))
    assert any(x["id"] == r["id"] for x in srv.get("/api/routines"))
    r2 = srv.put(f"/api/routines/{r['id']}", {"trigger": {"kind": "every", "minutes": 120}})
    assert r2["describe"] == "每 2 小时" and r2["name"] == "早报"
    p = srv.post(f"/api/routines/{r['id']}/pause")
    assert p["enabled"] is False
    assert srv.post(f"/api/routines/{r['id']}/resume")["enabled"] is True
    bad = srv.http.post(f"/api/bots/{b['id']}/routines", json={"name": "x", "prompt": "y", "trigger": {"kind": "every", "minutes": 1}})
    assert bad.status_code == 400 and bad.json()["detail"] == "间隔至少 5 分钟"
    bad = srv.http.post(f"/api/bots/{b['id']}/routines", json={"name": "x", "prompt": "y", "trigger": {"kind": "once", "at": "2020-01-01 10:00"}})
    assert bad.status_code == 400 and "已经过了" in bad.json()["detail"]
    assert srv.http.post(f"/api/routines/{r['id']}/nope").status_code == 404
    assert srv.delete(f"/api/routines/{r['id']}") == {"ok": True}
    assert srv.http.get(f"/api/routines/{r['id']}/runs").status_code == 404


def test_run_now_dispatches_task(srv, fake, ws):
    b = srv.make_bot("Runner")
    r = srv.post(f"/api/bots/{b['id']}/routines", {"name": "日报", "prompt": "写今天的日报", "trigger": {"kind": "daily", "times": ["08:00"]}})
    fake.script("worker", "今天的日报：一切正常")
    out = srv.post(f"/api/routines/{r['id']}/test")
    assert out["id"] == r["id"]
    res = until(lambda: srv.find(b["chat_id"], kind="result"), what="任务结果")
    assert "今天的日报：一切正常" in res[0]["content"]
    d = srv.find(b["chat_id"], kind="dispatch")[0]
    assert d["content"].startswith("[任务已派发] [定时任务 日报] 这是定时任务「日报」（每天 08:00）的一次运行。")
    goal = fake.bodies("worker")[0]["messages"][1]["content"]
    assert "要求：写今天的日报" in goal
    runs = until(lambda: [x for x in _runs(srv, r["id"]) if x["status"] == "done"], what="运行记录")
    assert runs[0]["source"] == "test" and runs[0]["task_id"] == d["id"]
    assert until(lambda: [e for e in ws.of("routines_changed") if e["bot_id"] == b["id"]], what="routines_changed")
    idle(srv)


def test_webhook_event_filters_and_quiet(srv, fake):
    b = srv.make_bot("Hooker")
    r = srv.post(f"/api/bots/{b['id']}/routines", {"name": "报错", "prompt": "看看报错",
                                                   "trigger": {"kind": "event", "source": "webhook", "include": ["bug"], "exclude": "dependabot"}})
    t = r["trigger"]
    assert r["category"] == "event" and t["source"] == "webhook" and len(t["key"]) >= 16
    assert t["include"] == ["bug"] and t["exclude"] == ["dependabot"]
    assert r["event"]["mode"] == "push" and r["event"]["hook"] == f"/hooks/{t['key']}"
    hook = f"/hooks/{t['key']}"
    assert srv.http.get(hook).json() == {"ok": True}
    assert srv.http.get("/hooks/" + "x" * 20).status_code == 404

    out = srv.http.post(hook, json={"title": "新功能上线"}).json()
    assert out == {"ok": True, "accepted": False, "stage": "rule", "reason": "没有命中关键词"}
    out = srv.http.post(hook, json={"title": "bug from dependabot"}).json()
    assert out["accepted"] is False and out["reason"] == "命中排除词"
    assert [x["status"] for x in _runs(srv, r["id"])][:2] == ["filtered", "filtered"]

    fake.script("worker", "[无新内容]")
    out = srv.http.post(hook, json={"title": "bug: 点保存没反应", "user": "alice"}).json()
    assert out["ok"] is True and out["accepted"] is True and out["task_id"]
    runs = until(lambda: [x for x in _runs(srv, r["id"]) if x["status"] == "quiet"], what="无新内容")
    assert runs[0]["source"] == "webhook" and runs[0]["detail"] == "没有新内容"
    goal = fake.bodies("worker")[0]["messages"][1]["content"]
    assert "<<<事件原文开始>>>" in goal and "bug: 点保存没反应" in goal and "事件来源：收到 Webhook" in goal
    assert "结果只写 [无新内容] 这几个字" in goal   # 只给 Worker 看的说明接在 goal 后面
    idle(srv)
    assert not srv.find(b["chat_id"], kind="result")   # 无新内容：不发结果
    d = srv.find(b["chat_id"], kind="dispatch")[0]
    assert d["meta"]["quiet"] is True and d["meta"]["status"] == "done"
    # 换地址
    nk = srv.post(f"/api/routines/{r['id']}/rekey")
    assert nk["trigger"]["key"] != t["key"]
    assert srv.http.post(hook, json={"title": "bug"}).status_code == 404


def test_github_ping_and_signature(srv, fake):
    b = srv.make_bot("Octo")
    r = srv.post(f"/api/bots/{b['id']}/routines", {"name": "GH", "prompt": "处理", "trigger": {"kind": "event", "source": "github",
                                                                                         "params": {"secret": "s3cret"}}})
    assert r["trigger"]["params"]["secret"].startswith("vault:event_")
    hook = f"/hooks/{r['trigger']['key']}"
    bad = srv.http.post(hook, json={"zen": "x"}, headers={"x-github-event": "ping", "x-hub-signature-256": "sha256=00"})
    assert bad.status_code == 401
    import hashlib, hmac, json
    body = json.dumps({"zen": "x", "hook_id": 7}).encode()
    sig = "sha256=" + hmac.new(b"s3cret", body, hashlib.sha256).hexdigest()
    ok = srv.http.post(hook, content=body, headers={"x-github-event": "ping", "x-hub-signature-256": sig, "content-type": "application/json"})
    assert ok.json() == {"ok": True, "accepted": False, "reason": "收到 GitHub 的 ping，地址配好了（7）"}
    assert _runs(srv, r["id"])[0]["status"] == "ignored"


def test_event_sources_listing(srv):
    cat = srv.get("/api/event-sources")
    ids = [s["id"] for s in cat]
    for sid in ("webhook", "github", "rss", "web_change", "site_down", "github_repo", "task_failed"):
        assert sid in ids, sid
    for s in cat:
        for k in ("id", "title", "description", "mode", "category", "icon", "fields", "every", "min_every", "setup",
                  "unavailable", "scenarios"):
            assert k in s, (s["id"], k)
    rss = next(s for s in cat if s["id"] == "rss")
    assert rss["mode"] == "poll" and rss["fields"][0]["key"] == "url" and rss["fields"][0]["label"] == "订阅源地址"
    assert rss["scenarios"] and {"source", "title", "name", "params", "prompt", "include", "exclude", "llm_filter"} <= set(rss["scenarios"][0])
    cats = [s["category"] for s in cat]
    order = ["开发", "资讯", "生活", "金融", "系统", "通用"]
    assert cats == sorted(cats, key=lambda c: order.index(c) if c in order else 99)


def test_routine_tools_in_chat(srv, fake, ws):
    b = srv.make_bot("Planner")
    fake.script("router:Planner", Call("create_routine", {"name": "喝水", "prompt": "提醒我喝水", "kind": "every", "minutes": 60}),
                Call("list_routines", {}), "建好了")
    ws.say(b["chat_id"], "每小时提醒我喝水")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="建好了"), what="回复")
    tool_out = [m["content"] for m in fake.bodies("router", "Planner")[-1]["messages"] if m["role"] == "tool"]
    assert tool_out[0].startswith("已建定时任务 #") and "「喝水」：每 1 小时，下次 " in tool_out[0]
    assert "定时任务：\n#" in tool_out[1] and "做什么：提醒我喝水" in tool_out[1]
    rs = srv.get(f"/api/bots/{b['id']}/routines")
    assert [x["name"] for x in rs] == ["喝水"]


# ---------- 要去看的来源（订阅源、网页）：本机起一个小网站，打开 tools.fetch_private ----------
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

PAGES: dict[str, tuple[str, bytes]] = {}


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


def _rss(items):
    xs = "".join(f"<item><title>{t}</title><link>https://ex.com/{i}</link><guid>{i}</guid><description>&lt;p&gt;正文 {t}&lt;/p&gt;</description></item>"
                 for i, t in items)
    return f'<?xml version="1.0" encoding="utf-8"?><rss version="2.0"><channel><title>测试源</title>{xs}</channel></rss>'.encode()


@pytest.fixture(scope="module")
def polling(llm):
    import conftest
    site = _site()
    s = conftest.Server(llm, config={"tools": {"fetch_private": True}})
    yield s, f"http://127.0.0.1:{site.server_address[1]}"
    s.stop()
    site.shutdown()


def test_rss_poll(polling, fake):
    srv, base = polling
    PAGES["/feed"] = ("application/rss+xml", _rss([("1", "第一篇")]))
    b = srv.make_bot("Reader")
    r = srv.post(f"/api/bots/{b['id']}/routines", {"name": "订阅", "prompt": "总结",
                                                   "trigger": {"kind": "event", "source": "rss", "params": {"url": base + "/feed"}, "every": 30}})
    assert r["trigger"]["every"] == 30 and r["describe"].startswith("订阅源有新文章：127.0.0.1:")
    out = srv.post(f"/api/routines/{r['id']}/check")
    assert out["note"] == "已记下现状，之后有新的才通知"
    assert out["event"]["checked"] and out["event"]["has_sample"] is True
    out = srv.post(f"/api/routines/{r['id']}/check")
    assert out["note"] == "没有新的"
    PAGES["/feed"] = ("application/rss+xml", _rss([("2", "第二篇 AI"), ("3", "第三篇"), ("1", "第一篇")]))
    fake.script("other", '{"handle": true, "reason": "相关"}')
    srv.put(f"/api/routines/{r['id']}", {"trigger": {"kind": "event", "source": "rss", "params": {"url": base + "/feed"}, "llm_filter": "只要 AI"}})
    fake.script("worker", "两篇新文章")
    out = srv.post(f"/api/routines/{r['id']}/check")
    assert out["note"] == "有新的，已经交给 Bot"
    res = until(lambda: srv.find(b["chat_id"], kind="result"), what="结果")
    assert "两篇新文章" in res[0]["content"]
    goal = fake.bodies("worker")[0]["messages"][1]["content"]
    assert "事件来源：订阅源有新文章（127.0.0.1:" in goal and "1. 第二篇 AI" in goal and "2. 第三篇" in goal and "正文 第三篇" in goal
    flt = fake.bodies("other")[0]
    assert flt["messages"][0]["content"].startswith("你是事件过滤器。") and "规则：只要 AI" in flt["messages"][1]["content"]
    idle(srv)


def test_web_change_poll(polling, fake):
    srv, base = polling
    PAGES["/page"] = ("text/html; charset=utf-8", "<html><title>通知</title><body><p>旧通知 A</p><p>访问 12</p></body></html>".encode())
    b = srv.make_bot("Watcher")
    r = srv.post(f"/api/bots/{b['id']}/routines", {"name": "公告", "prompt": "看看", "trigger": {"kind": "event", "source": "web_change",
                 "params": {"url": base + "/page", "ignore": "访问"}}})
    bad = srv.http.put(f"/api/routines/{r['id']}", json={"trigger": {"kind": "event", "source": "web_change", "params": {"mode": "appear"}}})
    assert bad.status_code == 400 and bad.json()["detail"] == "选了“出现”或“消失”，要填关键词"
    assert srv.post(f"/api/routines/{r['id']}/check")["note"] == "已记下现状，之后有新的才通知"
    PAGES["/page"] = ("text/html; charset=utf-8", "<html><title>通知</title><body><p>新通知 B</p><p>旧通知 A</p><p>访问 99</p></body></html>".encode())
    fake.script("worker", "多了一条通知")
    assert srv.post(f"/api/routines/{r['id']}/check")["note"] == "有新的，已经交给 Bot"
    until(lambda: srv.find(b["chat_id"], kind="result"), what="结果")
    goal = fake.bodies("worker")[0]["messages"][1]["content"]
    assert "网页变了：通知" in goal and "多了 1 行，少了 0 行" in goal and "+ 新通知 B" in goal and "访问" not in goal.split("<<<事件原文开始>>>")[1]
    idle(srv)
