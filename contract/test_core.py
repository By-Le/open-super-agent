"""核心接口：Bot、对话、讨论串、未读、任务（取消、过程）、记忆、新话题、群聊管理、静态文件。"""
import time

from conftest import idle, until
from fakellm_http import Call


def test_bot_crud(srv):
    b = srv.make_bot("  Crud Bot  ", "写周报")
    assert b["name"] == "Crud Bot" and b["instructions"] == "写周报" and b["chat_id"]
    assert b["avatar"]["shape"] and b["avatar"]["color"].startswith("#")
    u = srv.patch(f"/api/bots/{b['id']}", {"name": "Crud2", "avatar": {"shape": "star", "color": "#123456"}})
    assert u["name"] == "Crud2" and u["avatar"] == {"shape": "star", "color": "#123456"}
    chat = next(c for c in srv.get("/api/bootstrap")["chats"] if c["id"] == b["chat_id"])
    assert chat["title"] == "Crud2" and chat["kind"] == "direct" and chat["bot_ids"] == [b["id"]]
    r = srv.http.patch("/api/bots/999999", json={"name": "x"})
    assert r.status_code == 404 and "detail" in r.json()
    assert srv.delete(f"/api/bots/{b['id']}") == {"ok": True}
    assert not [x for x in srv.get("/api/bootstrap")["bots"] if x["id"] == b["id"]]


def test_primary_cannot_be_deleted(srv):
    p = next(b for b in srv.get("/api/bootstrap")["bots"] if b["is_primary"])
    r = srv.http.delete(f"/api/bots/{p['id']}")
    assert r.status_code == 400


def test_messages_paging_and_unread(srv, fake, ws):
    a = srv.make_bot("Pager")
    fake.script("router:Pager", "一", "二", "三")
    for i, text in enumerate(("问一", "问二", "问三")):
        ws.say(a["chat_id"], text)
        until(lambda: len(srv.find(a["chat_id"], role="assistant")) > i, what="回复")
    page = srv.get(f"/api/chats/{a['chat_id']}/messages", params={"limit": 2})
    assert [m["role"] for m in page] == ["user", "assistant"] and page[1]["content"] == "三"
    older = srv.get(f"/api/chats/{a['chat_id']}/messages", params={"limit": 2, "before": page[0]["id"]})
    assert [m["content"] for m in older] == ["问二", "二"]
    chat = next(c for c in srv.get("/api/bootstrap")["chats"] if c["id"] == a["chat_id"])
    assert chat["unread"] == 3 and chat["preview"] == "三"
    r = srv.post(f"/api/chats/{a['chat_id']}/seen", {"id": page[0]["id"]})
    assert r == {"unread": 1}
    assert until(lambda: [e for e in ws.of("chat_seen") if e["chat_id"] == a["chat_id"]], what="chat_seen 事件")
    assert srv.post(f"/api/chats/{a['chat_id']}/seen") == {"unread": 0}


def test_threads(srv, fake, ws):
    a = srv.make_bot("Threader")
    fake.script("router:Threader", "主回复", "讨论串里的回复")
    ws.say(a["chat_id"], "开个头")
    root = until(lambda: srv.find(a["chat_id"], role="assistant"), what="主回复")[0]
    t = srv.get(f"/api/chats/{a['chat_id']}/threads/{root['id']}")
    assert t["id"] is None and t["root"]["content"] == "主回复"
    th = srv.post(f"/api/chats/{a['chat_id']}/threads", {"root": root["id"]})
    assert th["id"] and th["root"]["id"] == root["id"]
    ws.say(th["id"], "针对这条")
    until(lambda: srv.find(th["id"], role="assistant"), what="讨论串里的回复")
    body = fake.bodies("router", "Threader")[-1]
    joined = "\n".join(str(m["content"]) for m in body["messages"])
    assert "以下是讨论串" in joined and "主回复" in joined
    lst = srv.get(f"/api/chats/{a['chat_id']}/threads")
    assert lst and lst[0]["root"] == root["id"] and lst[0]["count"] == 2 and lst[0]["unread"] == 1
    seen = srv.post(f"/api/threads/{th['id']}/seen")
    assert seen["unread"] == 0 and seen["parent"] == a["chat_id"]
    assert srv.get(f"/api/threads/{th['id']}")["root"] == root["id"]
    msgs = srv.get(f"/api/chats/{a['chat_id']}/messages")
    assert next(m for m in msgs if m["id"] == root["id"])["thread"]["count"] == 2
    assert srv.http.post(f"/api/chats/{th['id']}/reset").status_code == 400


def test_new_topic_cuts_context(srv, fake, ws):
    a = srv.make_bot("Topic")
    fake.script("router:Topic", "记住了", "新话题的回复")
    ws.say(a["chat_id"], "旧话题里的秘密")
    until(lambda: srv.find(a["chat_id"], role="assistant"), what="回复")
    mid = srv.post(f"/api/chats/{a['chat_id']}/reset")["id"]
    assert srv.find(a["chat_id"], kind="reset")[0]["id"] == mid
    ws.say(a["chat_id"], "新问题")
    until(lambda: len(srv.find(a["chat_id"], role="assistant")) == 2, what="第二条回复")
    joined = "\n".join(str(m["content"]) for m in fake.bodies("router", "Topic")[-1]["messages"])
    assert "旧话题里的秘密" not in joined and "新问题" in joined


def test_cancel_task(srv, fake, ws):
    a = srv.make_bot("Canceller")
    fake.script("router:Canceller", Call("dispatch", {"goal": "慢慢做"}), "派了")
    fake.script("worker", lambda body: (time.sleep(1.5), "做完了")[1])
    ws.say(a["chat_id"], "做个慢任务")
    task = until(lambda: srv.find(a["chat_id"], kind="dispatch"), what="派发")[0]
    until(lambda: [t for t in srv.get("/api/tasks", params={"bot_id": a["id"]}) if t["status"] == "running"], what="任务在跑")
    r = srv.post(f"/api/tasks/{task['id']}/cancel")
    assert "#" in r["result"]
    until(lambda: srv.find(a["chat_id"], kind="cancel"), what="取消记录")
    d = srv.get(f"/api/tasks/{task['id']}")
    assert d["task"]["status"] == "cancelled"
    assert any(s["kind"] == "note" and s["status"] == "cancelled" for s in d["steps"])
    assert srv.post(f"/api/tasks/{task['id']}/cancel")["result"].startswith("任务 #")
    time.sleep(1.8)
    assert not srv.find(a["chat_id"], kind="result")   # 取消后不会再出结果


def test_task_list_and_detail(srv, fake, ws):
    a = srv.make_bot("Lister")
    fake.script("router:Lister", Call("dispatch", {"goal": "查点东西"}), "好")
    fake.script("worker", "结论：没问题")
    ws.say(a["chat_id"], "查")
    res = until(lambda: srv.find(a["chat_id"], kind="result"), what="结果")[0]
    tid = res["meta"]["task_id"]
    lst = srv.get("/api/tasks", params={"chat_id": a["chat_id"]})
    assert lst[0]["id"] == tid and lst[0]["goal"] == "查点东西" and lst[0]["status"] == "done"
    d = srv.get(f"/api/tasks/{tid}")
    assert d["task"]["result"] == "结论：没问题" and d["task"]["tokens"] == 110 and "cost" in d["task"]
    kinds = [s["kind"] for s in d["steps"]]
    assert kinds[-1] == "answer"
    assert srv.http.get("/api/tasks/99999999").status_code == 404


def test_memories(srv, fake, ws):
    a = srv.make_bot("Memo")
    m = srv.post(f"/api/bots/{a['id']}/memories", {"text": "  用户喜欢   简短的回答 "})
    assert m["text"] == "用户喜欢 简短的回答" and m["shared"] is False and m["source"] == "manual"
    s = srv.post(f"/api/bots/{a['id']}/memories", {"text": "用户叫小王", "shared": True})
    assert s["shared"] is True and s["bot_id"] is None
    lst = srv.get(f"/api/bots/{a['id']}/memories")
    assert any(x["id"] == m["id"] for x in lst["own"]) and any(x["id"] == s["id"] for x in lst["shared"])
    u = srv.patch(f"/api/memories/{m['id']}", {"text": "用户喜欢先给结论"})
    assert u["text"] == "用户喜欢先给结论"
    fake.script("router:Memo", Call("remember", {"text": "用户在上海"}), "记住了")
    ws.say(a["chat_id"], "记住我在上海")
    until(lambda: srv.find(a["chat_id"], role="assistant"), what="回复")
    tool_msg = [x for x in fake.bodies("router", "Memo")[-1]["messages"] if x["role"] == "tool"]
    assert tool_msg[0]["content"].startswith("已记住 #") and "只有你知道" in tool_msg[0]["content"]
    sys = fake.bodies("router", "Memo")[0]["messages"][0]["content"]
    assert "用户喜欢先给结论" in sys and "用户叫小王" in sys
    assert srv.delete(f"/api/memories/{m['id']}") == {"ok": True}
    srv.delete(f"/api/memories/{s['id']}")


def test_group_management(srv):
    a, b, c = srv.make_bot("G1"), srv.make_bot("G2"), srv.make_bot("G3")
    g = srv.post("/api/groups", {"bot_ids": [a["id"], b["id"]]})
    assert g["kind"] == "group" and g["title"] == "G1、G2" and g["lead"] in (a["id"], b["id"])
    assert srv.http.post("/api/groups", json={"bot_ids": [a["id"]]}).status_code == 400
    u = srv.patch(f"/api/chats/{g['id']}", {"bot_ids": [a["id"], b["id"], c["id"]], "lead": c["id"], "title": "  "})
    assert u["lead"] == c["id"] and u["title"] == "G1、G2、G3"
    p = srv.post(f"/api/chats/{g['id']}/pin", {"pinned": True})
    assert p["pinned"]
    assert srv.post(f"/api/chats/{g['id']}/pin", {"pinned": False})["pinned"] == 0
    assert srv.http.delete(f"/api/chats/{a['chat_id']}").status_code == 400
    assert srv.delete(f"/api/chats/{g['id']}") == {"ok": True}


def test_mentions_pick_speaker(srv, fake, ws):
    a, b = srv.make_bot("Alpha"), srv.make_bot("Beta")
    g = srv.post("/api/groups", {"bot_ids": [a["id"], b["id"]], "lead": a["id"]})
    fake.script("router:Beta", "我是 Beta")
    ws.say(g["id"], "@Beta 你好")
    until(lambda: srv.find(g["id"], role="assistant", bot_id=b["id"]), what="Beta 回复")
    idle(srv)
    assert not fake.bodies("router", "Alpha")


def test_router_prompt_fixed_parts(srv, fake, ws):
    a = srv.make_bot("Prompty", "负责测试提示词")
    fake.script("router:Prompty", "好")
    ws.say(a["chat_id"], "你好")
    until(lambda: srv.find(a["chat_id"], role="assistant"), what="回复")
    body = fake.bodies("router", "Prompty")[-1]
    sys = body["messages"][0]["content"]
    assert sys.startswith("你是「Prompty」，用户的 AI 队友。负责测试提示词")
    assert "今天是 " in sys and "其他 Bot" in sys
    last = body["messages"][-1]["content"]
    assert last.startswith("你好\n\n（以下是系统附上的当前状态") and "暂无任务" in last
    names = [t["function"]["name"] for t in body["tools"]]
    assert names[:3] == ["browser_open", "browser_snapshot", "browser_click"] and "dispatch" in names
    assert body["stream"] is True and body["stream_options"] == {"include_usage": True}


def test_static_and_build(srv):
    r = srv.http.get("/api/build")
    assert r.status_code == 200 and "build" in r.json()
    r = srv.http.get("/")
    assert r.status_code == 200 and "text/html" in r.headers["content-type"]
    assert r.headers.get("cache-control") == "no-cache"
    r = srv.http.get("/api/me")
    assert r.json() == {"auth": False}


def test_cross_origin_write_refused(srv):
    r = srv.http.post("/api/bots", json={"name": "x"}, headers={"origin": "https://evil.example"})
    assert r.status_code == 403
    r = srv.http.get("/api/bootstrap", headers={"host": "evil.example"})
    assert r.status_code == 403
