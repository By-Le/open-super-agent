"""外部 Agent 的 MCP 端点（osa/agentapi.py）：token 的建、查、撤销，/mcp 的鉴权，每个工具，作者记成 agent:N，事件，锁定页面。

工具用 SDK 自带的 MCP 客户端调（走 httpx2 的 ASGITransport，和 Claude Code 一样经 HTTP），鉴权和老协议版本直接发 JSON-RPC。
"""
import httpx
import httpx2
import pytest
from conftest import settle
from mcp import Client
from mcp.client.streamable_http import streamable_http_client
from osa import agentapi, bots, dm, library, memory, pagecomments, pagedoc, security, server

DOC = "# 周报\n\n本周完成了三件事。\n\n- [ ] 联系销售"
RPC_HEADERS = {"accept": "application/json, text/event-stream", "content-type": "application/json"}


def http(**kw):
    return httpx.AsyncClient(transport=httpx.ASGITransport(app=server.app), base_url="http://127.0.0.1", **kw)


@pytest.fixture
def token():
    """建 token，测试完撤销。返回 make(name, scope) -> (记录, token 原文)。"""
    made: list[int] = []

    def make(name: str = "Claude Code", scope: str = "write"):
        t, tok = agentapi.create(name, scope)
        made.append(t["id"])
        return t, tok
    yield make
    for tid in made:
        agentapi.revoke(tid)


@pytest.fixture
def pages():
    made: list[int] = []
    yield made
    for pid in made:
        if library.get_page(pid, False):
            library.delete_page(pid)


@pytest.fixture
def bot(make_bot):
    made = []

    def make(name: str = "") -> dict:
        made.append(make_bot(name))
        return made[-1]
    yield make
    for b in made:
        bots.delete(b["id"])


class Agent:
    """用 SDK 的 MCP 客户端连 /mcp。"""

    def __init__(self, tok: str):
        self.hc = httpx2.AsyncClient(transport=httpx2.ASGITransport(app=server.app), base_url="http://127.0.0.1",
                                     headers={"authorization": f"Bearer {tok}"})
        self.client = Client(streamable_http_client("http://127.0.0.1/mcp", http_client=self.hc))

    async def __aenter__(self):
        await self.client.__aenter__()
        return self

    async def __aexit__(self, *exc):
        await self.client.__aexit__(*exc)
        await self.hc.aclose()

    async def tools(self) -> list[str]:
        return [t.name for t in (await self.client.list_tools()).tools]

    async def call(self, name: str, **args) -> str:
        r = await self.client.call_tool(name, args)
        text = "".join(c.text for c in r.content if getattr(c, "text", None))
        return ("ERROR " if r.is_error else "") + text


async def rpc(c, method: str, params: dict | None = None, tok: str | None = None, **headers):
    h = {**RPC_HEADERS, **headers}
    if tok is not None:
        h["authorization"] = f"Bearer {tok}"
    return await c.post("/mcp", headers=h, json={"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}})


# ---------- token ----------
def test_tokens(token):
    t, tok = token("  Claude   Code  ", "write")
    assert t["name"] == "Claude Code" and t["scope"] == "write" and t["revoked"] is None and t["last_used"] is None
    assert tok.startswith("osa_") and len(tok) > 40
    row = memory.conn.execute("select hash from agent_tokens where id=?", (t["id"],)).fetchone()
    assert row[0] != tok and tok not in row[0]   # 库里只有 sha256
    assert agentapi.check(tok)["id"] == t["id"]
    assert agentapi.get(t["id"])["last_used"]   # 用过就记下时间
    assert agentapi.check(tok + "x") is None and agentapi.check("") is None
    assert {"id": t["id"], "name": "Claude Code", "revoked": False} in agentapi.names()
    assert not any("hash" in x or "token" in x for x in agentapi.tokens())
    agentapi.revoke(t["id"])
    assert agentapi.check(tok) is None
    assert {"id": t["id"], "name": "Claude Code", "revoked": True} in agentapi.names()   # 撤销了名字还在：写过的段要显示
    assert agentapi.revoke(t["id"])["revoked"]   # 撤销两次没事
    with pytest.raises(ValueError):
        agentapi.create("   ", "write")
    with pytest.raises(ValueError):
        agentapi.create("x", "admin")
    with pytest.raises(LookupError):
        agentapi.revoke(999999)


async def test_settings_api(token, events):
    async with http() as c:
        r = await c.post("/api/agent-tokens", json={"name": "Codex", "scope": "read"})
        assert r.status_code == 200
        made = r.json()
        assert made["token"].startswith("osa_") and made["scope"] == "read"
        listed = (await c.get("/api/agent-tokens")).json()
        mine = next(x for x in listed if x["id"] == made["id"])
        assert "token" not in mine and "hash" not in mine   # 原文只在建的时候给一次
        assert any(e["agents"] for e in events.of("agents_changed"))
        assert (await c.post("/api/agent-tokens", json={"name": "", "scope": "read"})).status_code == 400
        assert (await c.delete(f"/api/agent-tokens/{made['id']}")).json()["revoked"]
        assert (await c.delete("/api/agent-tokens/999999")).status_code == 404
        boot = (await c.get("/api/bootstrap")).json()
        assert {"id": made["id"], "name": "Codex", "revoked": True} in boot["agents"]
        # 撤销了的能从列表里删掉，名字还在（写过的段要显示）；没撤销的不能删
        live, _ = token("还在用")
        assert (await c.post(f"/api/agent-tokens/{live['id']}/hide")).status_code == 400
        assert (await c.post(f"/api/agent-tokens/{made['id']}/hide")).json()["ok"]
        assert made["id"] not in [x["id"] for x in (await c.get("/api/agent-tokens")).json()]
        assert live["id"] in [x["id"] for x in (await c.get("/api/agent-tokens")).json()]
        assert {"id": made["id"], "name": "Codex", "revoked": True} in (await c.get("/api/bootstrap")).json()["agents"]
        assert agentapi.name(made["id"]) == "Codex"
        assert (await c.post("/api/agent-tokens/999999/hide")).status_code == 404


# ---------- 鉴权 ----------
async def test_auth(token, no_password):
    t, tok = token()
    async with http() as c:
        assert (await rpc(c, "tools/list")).status_code == 401
        r = await rpc(c, "tools/list", tok="osa_wrong")
        assert r.status_code == 401 and r.headers["www-authenticate"].startswith("Bearer")
        assert (await rpc(c, "tools/list", authorization=tok)).status_code == 401   # 不是 Bearer
        r = await rpc(c, "tools/list", tok=tok)
        assert r.status_code == 200 and "create_page" in r.text
        # Claude Code 这类客户端用的老协议版本：先 initialize
        r = await rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                        "clientInfo": {"name": "claude-code", "version": "1"}}, tok=tok)
        assert r.json()["result"]["serverInfo"]["name"] == "osa-library" and "read_page" in r.json()["result"]["instructions"]
        # 网页跨站发不来（来源不对）；没开登录时别人的域名也不行
        assert (await rpc(c, "tools/list", tok=tok, origin="https://evil.example")).status_code == 403
        assert (await rpc(c, "tools/list", tok=tok, host="evil.example")).status_code == 403
        agentapi.revoke(t["id"])
        assert (await rpc(c, "tools/list", tok=tok)).status_code == 401   # 撤销马上生效


async def test_auth_with_login(token, password):
    """开了登录：/mcp 不看 cookie、不跳登录页，只认 token；别的接口照样要登录。"""
    _, tok = token()
    async with http() as c:
        assert (await rpc(c, "tools/list")).status_code == 401
        assert (await rpc(c, "tools/list", tok=tok, host="osa.example.com")).status_code == 200
        assert (await c.get("/api/agent-tokens")).status_code == 401


async def test_demo_off(token, monkeypatch):
    _, tok = token()
    monkeypatch.setattr(security, "demo", lambda: True)
    async with http() as c:
        assert (await rpc(c, "tools/list", tok=tok)).status_code == 404
        assert (await c.get("/api/agent-tokens")).status_code == 403
        assert (await c.post("/api/agent-tokens", json={"name": "x", "scope": "read"})).status_code == 403


async def test_read_only(token, pages, bot):
    _, tok = token("Reader", "read")
    pid = library.save_page(bot()["id"], "周报", DOC)["id"]
    pages.append(pid)
    async with Agent(tok) as a:
        assert set(await a.tools()) == {"search_pages", "list_pages", "read_page", "list_comments", "db_query"}
        assert "周报" in await a.call("read_page", page_id=pid)
        out = await a.call("create_page", title="x", content="y")   # 看不到，硬调也不行
        assert out.startswith("ERROR") and "只读" in out
        assert "只读" in await a.call("comment_page", page_id=pid, text="hi")
    assert len(pagecomments.comments(pid)) == 0


# ---------- 工具 ----------
async def test_create_read_edit(token, pages, events):
    t, tok = token("Claude Code")
    by = f"agent:{t['id']}"
    async with Agent(tok) as a:
        assert set(await a.tools()) == {"search_pages", "list_pages", "read_page", "create_page", "edit_page", "rewrite_page",
                                        "list_comments", "comment_page", "db_query", "db_create", "db_schema", "db_add", "db_update"}
        out = await a.call("create_page", title="调研：向量库", content="# 结论\n\n用 sqlite-vec。\n\n- 便宜\n- 够快")
        assert out.startswith("已建页面") and "http://127.0.0.1/library/pages/" in out
        pid = int(out.split("#")[1].split("「")[0])
        pages.append(pid)
        p = library.get_page(pid, False)
        assert p["agent_id"] == t["id"] and p["bot_id"] is None and p["version"] == 1
        _, blks = pagedoc.load(pid)
        assert {b["by"] for b in blks} == {by}
        assert events.of("pages_changed")

        text = await a.call("read_page", page_id=pid)
        assert "版本 1" in text and "「Claude Code（外部 Agent）」建的" in text
        ids = [b["id"] for b in blks]
        assert all(f"⟦{i}⟧" in text for i in ids)

        out = await a.call("edit_page", page_id=pid, version=1, ops=[
            {"op": "replace", "block": ids[1], "markdown": "用 sqlite-vec（已经在用 sqlite）。"},
            {"op": "append", "markdown": "- 不用另起服务"}])
        assert out.startswith("已改页面") and "版本 2" in out
        _, blks = pagedoc.load(pid)
        assert blks[1]["md"] == "用 sqlite-vec（已经在用 sqlite）。" and blks[1]["by"] == by and blks[-1]["md"] == "- 不用另起服务"
        ev = [e for e in events.of("page_changed") if e["page_id"] == pid]
        assert ev and ev[-1]["by"] == by and ids[1] in ev[-1]["changed"]

        # 用户在网页上改了第二段，Agent 还拿着版本 2 去改它：冲突，告诉它现在的内容
        pagedoc.apply(pid, 2, [{"op": "replace", "block": ids[1], "markdown": "用户改的"}], "user")
        out = await a.call("edit_page", page_id=pid, version=2, ops=[{"op": "replace", "block": ids[1], "markdown": "又改"}])
        assert "没改" in out and "用户改的" in out and "version=3" in out
        out = await a.call("edit_page", page_id=pid, version=2, ops=[{"op": "delete", "block": ids[0]}])   # 别的段照样能改
        assert out.startswith("已改页面")
        out = await a.call("edit_page", page_id=pid, version=4, ops=[{"op": "replace", "markdown": "x"}])
        assert "没改" in out and "block" in out
        out = await a.call("edit_page", page_id=pid, version=4, ops=[{"op": "bogus"}])
        assert out.startswith("ERROR")   # 不在 enum 里：参数检查就挡住
    await settle()


async def test_rewrite(token, pages, bot, events):
    t, tok = token("Codex")
    b = bot()
    pid = library.save_page(b["id"], "周报", DOC)["id"]
    pages.append(pid)
    _, blks = pagedoc.load(pid)
    async with Agent(tok) as a:
        out = await a.call("rewrite_page", page_id=pid, version=1, content=DOC.replace("三件事", "四件事"), title="周报（改）")
        assert out.startswith("已保存页面") and "版本 2" in out
        p, now = pagedoc.load(pid)
        assert p["title"] == "周报（改）"
        assert [x["id"] for x in now] == [x["id"] for x in blks]   # 按位置保住编号
        assert now[0]["by"] == f"bot:{b['id']}" and now[1]["by"] == f"agent:{t['id']}"   # 没变的段还是原来的作者
        assert any(e["page_id"] == pid and e["changed"] == [] for e in events.of("page_changed"))
        assert "没有变化" in await a.call("rewrite_page", page_id=pid, version=2, content=p["content"])
        pagedoc.apply(pid, 2, [{"op": "replace", "block": blks[1]["id"], "markdown": "用户改的"}], "user")
        out = await a.call("rewrite_page", page_id=pid, version=2, content=DOC)
        assert "没保存" in out and "用户改的" in out
        assert "没有页面" in await a.call("rewrite_page", page_id=999999, version=1, content="x")


async def test_locked_and_html(token, pages, bot):
    _, tok = token()
    b = bot()
    pid = library.save_page(b["id"], "周报", DOC)["id"]
    html = library.save_page(b["id"], "看板", "<h1>hi</h1>", "html")["id"]
    pages.extend([pid, html])
    library.update_page(pid, locked=True)
    async with Agent(tok) as a:
        assert "已锁定" in await a.call("read_page", page_id=pid)
        assert "锁定" in await a.call("edit_page", page_id=pid, version=1, ops=[{"op": "append", "markdown": "x"}])
        assert "锁定" in await a.call("rewrite_page", page_id=pid, version=1, content="x")
        assert "已在页面" in await a.call("comment_page", page_id=pid, text="这里要改")   # 锁了还能评论
        assert "hi" in await a.call("read_page", page_id=html)
        assert "html" in await a.call("edit_page", page_id=html, version=1, ops=[{"op": "append", "markdown": "x"}])
        assert "html" in await a.call("rewrite_page", page_id=html, version=1, content="x")
    assert library.get_page(pid, False)["version"] == 1


async def test_list_and_search(token, pages, bot):
    _, tok = token()
    b = bot()
    top = library.save_page(b["id"], "项目甲", "# 项目甲\n\n总览")["id"]
    kid = library.save_page(b["id"], "甲的会议纪要", "讨论了猕猴桃供应链", parent_id=top)["id"]
    pages.extend([kid, top])
    async with Agent(tok) as a:
        out = await a.call("list_pages")
        line_top = next(ln for ln in out.splitlines() if f"#{top}《" in ln)
        line_kid = next(ln for ln in out.splitlines() if f"#{kid}《" in ln)
        assert line_kid.startswith("  - ") and not line_top.startswith(" ") and "版本 1" in line_kid
        out = await a.call("list_pages", parent_id=top)
        assert f"#{kid}《甲的会议纪要》" in out and f"#{top}《" in out.splitlines()[0] and "1 个页面" in out
        assert "没有页面" in await a.call("list_pages", parent_id=999999)
        out = await a.call("search_pages", query="猕猴桃")
        assert f"#{kid}《甲的会议纪要》" in out and "/library/pages/" in out
        assert await a.call("search_pages", query="完全不存在的词zzqx") == "没找到相关的页面"


async def test_comments(token, pages, bot, monkeypatch, events):
    """外面 Agent 的评论不叫醒 Bot；能回复用户的评论、标成已解决。"""
    t, tok = token("Claude Code")
    woke = []
    monkeypatch.setattr(dm, "wake", lambda *a, **k: woke.append(a))
    b = bot()
    pid = library.save_page(b["id"], "周报", DOC)["id"]
    pages.append(pid)
    ids = [x["id"] for x in pagedoc.load(pid)[1]]
    root = pagecomments.add(pid, "user", "数字对吗？", ids[1])
    async with Agent(tok) as a:
        out = await a.call("list_comments", page_id=pid)
        assert f"#{root['id']} ⟦{ids[1]}⟧ 用户：数字对吗？" in out
        out = await a.call("comment_page", page_id=pid, text="核对过了，是三件", reply_to=root["id"], resolve=True)
        assert "回复评论" in out and "已解决" in out
        assert "都解决了" in await a.call("list_comments", page_id=pid)
        out = await a.call("list_comments", page_id=pid, include_resolved=True)
        assert "（已解决）" in out and "Claude Code（外部 Agent）：核对过了" in out
        out = await a.call("comment_page", page_id=pid, text="第一段也看看", block=ids[0])
        assert "写了评论" in out
        assert "没写" in await a.call("comment_page", page_id=pid, text="x", block="nope")
    mine = [c for c in pagecomments.comments(pid) if c["by"] == f"agent:{t['id']}"]
    assert len(mine) == 2 and not woke and all(not c["woke"] for c in mine)
    assert any(e["page_id"] == pid for e in events.of("page_comments"))
    assert "Claude Code（外部 Agent）" in pagedoc.render(pid)   # Bot 读页面时也看到是谁说的


async def test_db_tools(token, pages, events):
    """数据库：建库、加行、查、改属性、改表结构，作者记成 agent:N，发 db_changed。"""
    from osa import dbs
    t, tok = token("Codex")
    async with Agent(tok) as a:
        out = await a.call("db_create", title="任务", properties=[{"name": "状态", "type": "status"}, {"name": "分", "type": "number"}])
        db_id = int(out.split("#")[1].split("「")[0])
        pages.append(db_id)
        out = await a.call("db_add", db_id=db_id, rows=[{"title": "写文档", "props": {"状态": "进行中", "分": 3}}, {"title": "测试"}])
        assert "加了 2 行" in out
        rid = int(out.split("#")[2].split(" ")[0])
        assert dbs.get_row(rid)["created_by"] == f"agent:{t['id']}"
        out = await a.call("db_query", db_id=db_id, filter=[{"prop": "状态", "op": "eq", "value": "进行中"}])
        assert "符合条件的 1 行" in out and "| 写文档 | 进行中 | 3 |" in out
        out = await a.call("db_update", rows=[{"row_id": rid, "props": {"状态": "已完成"}, "version": 1}])
        assert "改了 状态，现在是版本 2" in out
        assert "优先级" in await a.call("db_schema", db_id=db_id, ops=[{"op": "add", "name": "优先级", "type": "select", "options": ["高"]}])
        assert "数据库《任务》" in await a.call("read_page", page_id=db_id)
        out = await a.call("db_schema", db_id=db_id, ops=[{"op": "add", "name": "双倍", "type": "formula", "formula": 'prop("分") * 2'},
                                                          {"op": "add", "name": "编号", "type": "unique_id", "prefix": "T-"}])
        assert "双倍" in out and "T-" in out
        out = await a.call("db_query", db_id=db_id, props=["分", "双倍", "编号"])
        assert "| 写文档 | 3 | 6 | T-1 |" in out
    assert any(e["db_id"] == db_id and e["by"] == f"agent:{t['id']}" for e in events.of("db_changed"))
    _, ro = token("Reader", "read")
    async with Agent(ro) as a:
        assert "只读" in await a.call("db_add", db_id=db_id, rows=[{"title": "x"}])
        assert "写文档" in await a.call("db_query", db_id=db_id)
