"""冒烟：私聊派任务、群聊交接、Bot 之间私信、应用的写操作弹确认卡片。和 tests/test_smoke.py 同样的场景，走真的接口。"""
import time

from conftest import idle, until
from fakellm_http import Call


def test_bootstrap_shape(srv):
    b = srv.get("/api/bootstrap")
    for k in ("bots", "chats", "settings", "shapes", "colors", "tasks", "approvals", "threads", "apps", "catalog",
              "categories", "oauth_callback", "teach", "auth", "onboarding", "build"):
        assert k in b, k
    assert b["bots"] and b["bots"][0]["is_primary"]   # 第一次启动建好默认的主 Bot


def test_dispatch_in_direct_chat(srv, fake, ws):
    a = srv.make_bot("Scout")
    fake.script("router:Scout", Call("dispatch", {"goal": "查一下北京明天的天气"}), "已经派出去了")
    fake.script("worker", "北京明天晴，最高 12 度")

    ws.say(a["chat_id"], "帮我查下北京明天天气")
    result = until(lambda: srv.find(a["chat_id"], kind="result"), what="任务结果")

    assert "北京明天晴" in result[0]["content"]
    assert srv.find(a["chat_id"], role="assistant", text="已经派出去了")
    assert srv.find(a["chat_id"], kind="dispatch", text="查一下北京明天的天气")
    worker = fake.bodies("worker")
    assert worker and worker[0]["messages"][1] == {"role": "user", "content": "查一下北京明天的天气"}
    # 网页收到了流式输出、消息、任务状态
    assert until(lambda: [e for e in ws.of("stream") if e.get("delta")], what="流式正文")
    assert ws.of("stream_start") and ws.of("stream_end")
    assert until(lambda: [e for e in ws.of("task") if e["task"]["status"] == "done"], what="任务完成事件")
    msgs = [e for e in ws.of("message") if e["chat_id"] == a["chat_id"]]
    assert {m["message"]["role"] for m in msgs} >= {"user", "assistant", "task"}
    assert all("unread" in m for m in msgs)
    idle(srv)


def test_group_handoff_after_task(srv, fake, ws):
    a, b = srv.make_bot("Finder"), srv.make_bot("Writer")
    group = srv.post("/api/groups", {"bot_ids": [a["id"], b["id"]], "lead": a["id"]})
    fake.script("router:Finder", Call("dispatch", {"goal": "查资料", "handoff_to": "Writer", "handoff_note": "写个总结"}),
                "我先查，查完交给 Writer")
    fake.script("worker", "资料：三条要点")
    fake.script("router:Writer", "总结：三条要点都很重要")

    ws.say(group["id"], "@Finder 查资料，查完交给 @Writer 写总结")
    until(lambda: srv.find(group["id"], role="assistant", bot_id=b["id"]), what="Writer 的总结")
    idle(srv)

    assert len(fake.bodies("router", "Writer")) == 1
    assert srv.find(group["id"], kind="handoff", text="任务 #")
    seen = "\n".join(str(m.get("content")) for m in fake.bodies("router", "Writer")[0]["messages"])
    assert "资料：三条要点" in seen and "写个总结" in seen


def test_group_handoff_when_task_finishes_first(srv, fake, ws):
    """任务比派它的这一轮先做完：被点名的接手人还是只说一次（交接时），不会先按点名说一次。"""
    a, b = srv.make_bot("Quick"), srv.make_bot("Taker")
    group = srv.post("/api/groups", {"bot_ids": [a["id"], b["id"]], "lead": a["id"]})

    def slow(body):   # Quick 派完任务后的那次请求：等任务结果出来、交接排上了再回
        until(lambda: srv.find(group["id"], kind="result"), what="任务结果")
        time.sleep(0.2)
        return "我先查，查完交给 Taker"
    fake.script("router:Quick", Call("dispatch", {"goal": "查资料", "handoff_to": "Taker", "handoff_note": "写个总结"}), slow)
    fake.script("worker", "资料：两条要点")
    fake.script("router:Taker", "总结：两条要点")

    ws.say(group["id"], "@Quick 查资料，查完交给 @Taker 写总结")
    until(lambda: srv.find(group["id"], role="assistant", bot_id=b["id"]), what="Taker 的总结")
    idle(srv)
    time.sleep(0.5)
    assert len(fake.bodies("router", "Taker")) == 1
    assert len(srv.find(group["id"], role="assistant", bot_id=b["id"])) == 1
    assert srv.find(group["id"], kind="handoff", text="任务 #")


def test_dm_reply_comes_back(srv, fake, ws):
    nova, post = srv.make_bot("Nova2"), srv.make_bot("Postman")
    fake.script("router:Nova2", Call("message_bot", {"to": "Postman", "text": "看看有没有 Anthropic 的邮件"}),
                "已经让 Postman 去看了", "Postman 说没有新邮件")
    fake.script("router:Postman", "没有 Anthropic 的新邮件")

    ws.say(nova["chat_id"], "让 Postman 看看有没有 Anthropic 的邮件")
    until(lambda: srv.find(nova["chat_id"], role="assistant", text="Postman 说没有新邮件"), what="Nova 转告回信")

    assert srv.find(post["chat_id"], kind="dm", text="看看有没有 Anthropic 的邮件")
    assert srv.find(nova["chat_id"], kind="dm_reply", text="没有 Anthropic 的新邮件")
    assert srv.find(post["chat_id"], kind="dm_reply")[0]["meta"].get("sent")


def test_app_write_needs_approval(srv, fake, ws):
    until(lambda: any(a["name"] == "demo" and a.get("state") == "connected" for a in srv.get("/api/apps")["apps"]),
          timeout=30, what="假应用连上")
    a = srv.make_bot("Keeper")
    fake.script("router:Keeper", Call("dispatch", {"goal": "记一条笔记：明天开会"}), "这就记")
    fake.script("worker", [Call("demo__search_notes", {"query": "开会"})], Call("demo__create_note", {"text": "明天开会"}), "记好了")

    ws.say(a["chat_id"], "帮我记一条笔记：明天开会")
    card = until(lambda: [e for e in ws.of("approval") if e.get("status") == "pending"], what="确认卡片")
    assert card[0]["chat_id"] == a["chat_id"]
    assert card[0]["tool"] == "demo__create_note"
    ws.send({"type": "approve", "id": card[0]["id"], "ok": True})
    result = until(lambda: srv.find(a["chat_id"], kind="result"), what="任务结果")
    assert "记好了" in result[0]["content"]
    tool_out = [m for b in fake.bodies("worker") for m in b["messages"] if m["role"] == "tool"]
    assert any("找到 1 条笔记：开会 相关" in m["content"] for m in tool_out)
    assert any("已创建笔记：明天开会" in m["content"] for m in tool_out)
    # 任务过程：两步工具都记下了
    detail = srv.get(f"/api/tasks/{result[0]['meta']['task_id']}")
    tools = [s for s in detail["steps"] if s["kind"] == "tool"]
    assert len(tools) == 2 and [s["status"] for s in tools] == ["done", "done"]
    with srv.db() as c:
        names = [r[0] for r in c.execute("select name from task_steps where task_id=? and kind='tool' order by id",
                                         (result[0]["meta"]["task_id"],))]
    assert names == ["demo__search_notes", "demo__create_note"]
    idle(srv)
