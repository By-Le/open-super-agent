"""给 OSA 外面的 Agent（Claude Code、Codex……）读写资料库：/mcp 上的 MCP 端点（Streamable HTTP，无状态、JSON 响应）。

- 鉴权只认 Authorization: Bearer <token>，不看登录 cookie（auth.py 对 /mcp 放行，到这里查 token；Host、来源照样检查）。
  token 在 设置 → 外部 Agent 里建，只在建的时候显示一次，库里只存 sha256。scope：read（只读）或 write（能写）；
  只读的 token 看不到写的工具。每个请求都重新查 token，撤销马上生效。演示服务器（security.demo）上整个关掉。
- 作者记成 agent:<token 编号>，名字是建 token 时填的（撤销了也留着：页面上的作者条、评论照样显示它的名字）。
- 外面 Agent 写的评论不叫醒 Bot（pagecomments.tool_comment 走 add，不走 post）：只有用户写的评论叫醒 Bot。
  锁定的页面只能读、评论；html 页面只能读。
- 工具都复用 Bot 用的那几个函数（search / library / pagedoc / pagecomments），结果和 Bot 看到的一样是中文；
  写了照样发 page_changed / pages_changed / page_comments，网页上开着的页面实时看到。
- 无状态：每个请求单独处理，不记会话，挂在反代后面、服务重启都不影响已经接上的 Agent。
"""
import hashlib
import json
import logging
import secrets
import time
from typing import Annotated, Literal

from mcp.server.mcpserver import Context, MCPServer
from mcp.server.mcpserver.exceptions import ToolError
from mcp.server.streamable_http_manager import StreamableHTTPSessionManager
from mcp.server.transport_security import TransportSecuritySettings
from mcp_types import ListToolsResult, ToolAnnotations
from pydantic import BaseModel, Field

from . import i18n, library, media, pagecomments, pagedoc, registry, search, security
from .memory import conn

conn.execute("""create table if not exists agent_tokens(
  id integer primary key, name text, hash text unique, scope text, created real, last_used real, revoked real)""")
conn.commit()

PATH = "/mcp"
SCOPES = ("read", "write")
PREFIX = "osa_"
MAX_TOKENS = 50      # 没撤销的
MAX_NAME = 40
TOUCH_EVERY = 60     # “最后用过”隔这么久才写一次库
LIST_LIMIT = 300     # list_pages 最多列这么多行
_COLS = "id, name, scope, created, last_used, revoked"


# ---------- token ----------
def _hash(token: str) -> str:
    return hashlib.sha256(token.encode()).hexdigest()


def _row(r) -> dict:
    i, name, scope, created, last_used, revoked = r
    return {"id": i, "name": name, "scope": scope, "created": created, "last_used": last_used, "revoked": revoked}


def tokens() -> list[dict]:
    """设置页的列表：没撤销的在前，新的在前。"""
    return [_row(r) for r in conn.execute(f"select {_COLS} from agent_tokens order by revoked is not null, id desc")]


def get(tid: int) -> dict | None:
    r = conn.execute(f"select {_COLS} from agent_tokens where id=?", (tid,)).fetchone()
    return _row(r) if r else None


def create(name: str, scope: str) -> tuple[dict, str]:
    """新建一个 token：返回 (记录, token 原文)。原文只有这一次，库里只存 sha256。"""
    name = " ".join(str(name or "").split())[:MAX_NAME]
    if not name:
        raise ValueError(i18n.t("给它起个名字，比如 Claude Code"))
    if scope not in SCOPES:
        raise ValueError(i18n.t("权限只能是只读或可写"))
    if conn.execute("select count(*) from agent_tokens where revoked is null").fetchone()[0] >= MAX_TOKENS:
        raise ValueError(i18n.t("最多 {n} 个外部 Agent，先撤销一些", n=MAX_TOKENS))
    token = PREFIX + secrets.token_urlsafe(32)
    tid = conn.execute("insert into agent_tokens(name, hash, scope, created) values(?,?,?,?)",
                       (name, _hash(token), scope, time.time())).lastrowid
    conn.commit()
    return get(tid), token


def revoke(tid: int) -> dict:
    t = get(tid)
    if not t:
        raise LookupError(i18n.t("没有这个外部 Agent"))
    if t["revoked"] is None:
        conn.execute("update agent_tokens set revoked=? where id=?", (time.time(), tid))
        conn.commit()
    return get(tid)


def check(token: str) -> dict | None:
    """token 对得上、没撤销：返回记录，顺手记下最后用过的时间。"""
    if not token:
        return None
    r = conn.execute(f"select {_COLS} from agent_tokens where hash=? and revoked is null", (_hash(token),)).fetchone()
    if not r:
        return None
    t, now = _row(r), time.time()
    if not t["last_used"] or now - t["last_used"] > TOUCH_EVERY:
        conn.execute("update agent_tokens set last_used=? where id=?", (now, t["id"]))
        conn.commit()
        t["last_used"] = now
    return t


def names() -> list[dict]:
    """给网页显示作者用（bootstrap 的 agents）：撤销了的也在，它写过的段还要显示名字。"""
    return [{"id": r[0], "name": r[1], "revoked": r[2] is not None} for r in conn.execute("select id, name, revoked from agent_tokens order by id")]


def name(aid: int) -> str | None:
    r = conn.execute("select name from agent_tokens where id=?", (aid,)).fetchone()
    return r[0] if r else None


# ---------- MCP 工具 ----------
INSTRUCTIONS = """这是 Open Super Agent（OSA）的资料库：用户和 OSA 里的 Bot 一起写的文档（Markdown 页面，可以有子页面）。
你写的内容署你的名字（建 token 时起的），用户在网页上实时看到，也能在段落上给你写评论。

怎么用：
- 找页面用 search_pages（按意思查）或 list_pages（看目录）；读用 read_page，它会给出版本号和每段前面的段落编号 ⟦xxxx⟧。
- 改页面优先用 edit_page 按段改：只动要改的段，不会冲掉用户、Bot 刚改的地方。整页重写才用 rewrite_page。
- 都要带上 read_page 看到的版本号。你要改的段在你读过之后被别人改了，这一批都不写，结果里会告诉你它现在的内容，按新的版本号再提交就行。
- 段落编号只给 edit_page / comment_page 用，不是正文，写内容时不要带上。
- 用户在页面上的评论用 list_comments 看，用 comment_page 回复、标成已解决。你写的评论不会叫醒 OSA 里的 Bot。
- 用户锁定的页面只能读和评论。"""

WRITE_TOOLS = {"create_page", "edit_page", "rewrite_page", "comment_page"}
_RO = ToolAnnotations(read_only_hint=True, open_world_hint=False)
_RW = ToolAnnotations(read_only_hint=False, destructive_hint=False, open_world_hint=False)


class _Server(MCPServer):
    async def _handle_list_tools(self, ctx, params):
        res = await super()._handle_list_tools(ctx, params)
        if _scope_of(getattr(ctx, "request", None)) != "write":   # 只读的 token 看不到写的工具
            res = ListToolsResult(tools=[t for t in res.tools if t.name not in WRITE_TOOLS])
        return res


def _scope_of(req) -> str:
    a = req.scope.get("osa.agent") if req is not None else None
    return a["scope"] if a else ""


def _agent(ctx: Context, write: bool = False) -> dict:
    req = ctx.request_context.request
    a = req.scope.get("osa.agent") if req is not None else None
    if not a:
        raise ToolError("没有带 token")
    if write and a["scope"] != "write":
        raise ToolError("这个 token 只读，不能改资料库。要写的话在 OSA 的 设置 → 外部 Agent 里建一个可写的")
    return a


def _base(ctx: Context) -> str:
    """网页地址的开头（反代后面按 X-Forwarded-Proto 算 https），拼出能直接打开的页面链接。"""
    req = ctx.request_context.request
    if req is None:
        return ""
    scheme = (req.headers.get("x-forwarded-proto") or req.url.scheme or "http").split(",")[0].strip()
    return f"{scheme}://{req.headers.get('host') or req.url.netloc}"


def _url(ctx: Context, pid: int) -> str:
    return _base(ctx) + library.link(pid)


def _by(a: dict) -> str:
    return f"agent:{a['id']}"


def _when(ts: float) -> str:
    return time.strftime("%Y-%m-%d %H:%M", time.localtime(ts))


def _quiet_server() -> _Server:
    """MCPServer 建的时候会 logging.basicConfig(INFO)：OSA 没配根日志器，不还原的话所有库的 INFO 日志都会打出来
    （httpx 会把每个出站请求的地址都记下来，有的服务商把 key 放在地址里）。每个请求一行的“Terminating session”也压掉。"""
    root = logging.getLogger()
    level, handlers = root.level, list(root.handlers)
    s = _Server("osa-library", title="OSA 资料库", instructions=INSTRUCTIONS)
    root.setLevel(level)
    root.handlers[:] = handlers
    for name in ("mcp.server.streamable_http", "mcp.server.streamable_http_manager"):
        logging.getLogger(name).setLevel(logging.WARNING)
    return s


server = _quiet_server()


def _tool(description: str, read_only: bool):
    """结果只给文字（和 Bot 看到的一样），不再附一份 structuredContent：同样的内容发两遍，白占对方的上下文。"""
    return server.tool(description=description, annotations=_RO if read_only else _RW, structured_output=False)


@_tool("按意思查资料库里的页面（标题和正文），返回页面编号、标题、命中的那一小段。看全文用 read_page。", read_only=True)
async def search_pages(query: Annotated[str, Field(description="要找什么，用自然的说法；可以多写几个同义的词")],
                       ctx: Context,
                       limit: Annotated[int, Field(description="最多返回几个，默认 10", ge=1, le=30)] = 10) -> str:
    _agent(ctx)
    hits = await search.search(query, None, ("page",), limit)
    if not hits:
        return "没找到相关的页面"
    return "\n".join(f"- #{h['ref']}《{h['title']}》{_url(ctx, h['ref'])}：{h['snippet']}" for h in hits)


@_tool("列出资料库的页面目录（缩进的是子页面），带页面编号、格式、版本号和更新时间。给 parent_id 只列那个页面下面的。", read_only=True)
async def list_pages(ctx: Context,
                     parent_id: Annotated[int | None, Field(description="可选：只列这个页面下面的子页面")] = None) -> str:
    _agent(ctx)
    pages = library.list_pages()   # 置顶的在前、新的在前：最上层就按这个顺序，和网页上的页面树一样
    by_id = {p["id"]: p for p in pages}
    kids: dict = {}
    for p in pages:
        up = p["parent_id"] if p["parent_id"] in by_id else None
        kids.setdefault(up, []).append(p)
    for k, v in kids.items():
        if k is not None:
            v.sort(key=lambda p: (p["pos"], p["id"]))
    if parent_id is not None and parent_id not in by_id:
        return f"没有页面 #{parent_id}"
    lines: list[str] = []
    total = [0]

    def walk(pid, depth: int, seen: frozenset):
        for p in kids.get(pid, []):
            total[0] += 1
            if len(lines) < LIST_LIMIT:
                extra = " · html" + (" 小应用" if p["app"] else "") if p["format"] == "html" else f" · 版本 {p['version']}"
                lines.append(f"{'  ' * depth}- #{p['id']}《{p['title']}》{extra}{' · 已锁定' if p['locked'] else ''}"
                             f" · {_when(p['updated'])} 更新")
            if p["id"] not in seen:
                walk(p["id"], depth + 1, seen | {p["id"]})

    walk(parent_id, 0, frozenset())
    if parent_id is not None:
        up = "".join(f"#{x['id']}《{x['title']}》 / " for x in library.family(parent_id)["ancestors"])
        head = f"{up}#{parent_id}《{by_id[parent_id]['title']}》下面有 {total[0]} 个页面" + ("：" if total[0] else "")
    else:
        head = f"资料库共 {total[0]} 个页面" + ("（缩进的是子页面）：" if total[0] else "")
    more = f"\n…还有 {total[0] - len(lines)} 个没列出来，用 parent_id 分开看，或者用 search_pages 找" if total[0] > len(lines) else ""
    return head + ("\n" + "\n".join(lines) if lines else "") + more


@_tool("读资料库里的一个页面（html 页面只给文字）。markdown 页面带版本号，每段前面一行是段落编号 ⟦xxxx⟧，"
       "改页面、写评论时用；末尾附上没处理的评论。页面太长时会告诉你从哪一段接着读。", read_only=True)
async def read_page(page_id: int, ctx: Context,
                    start: Annotated[str | None, Field(description="可选：从这个段落编号开始读（页面太长、上次没读完时）")] = None) -> str:
    _agent(ctx)
    return library.tool_read_page({"page_id": page_id, "start": start})


@_tool("在资料库里新建一个 Markdown 页面，返回页面编号和网页链接。给 parent_id 就放在那个页面下面当子页面。", read_only=False)
async def create_page(title: str,
                      content: Annotated[str, Field(description="页面全文，Markdown")],
                      ctx: Context,
                      parent_id: Annotated[int | None, Field(description="可选：放在哪个页面下面当子页面")] = None) -> str:
    a = _agent(ctx, write=True)
    try:
        p = library.save_page(None, title, media.embed(content), "markdown", by=_by(a), parent_id=parent_id)
    except (ValueError, TypeError) as e:
        return f"没建：{e}"
    await registry.emit("pages_changed")
    return (f"已建页面 #{p['id']}「{p['title']}」（版本 {p['version']}），网页上打开：{_url(ctx, p['id'])}。"
            "接着改用 read_page 看段落编号，再用 edit_page 按段改。")


class EditOp(BaseModel):
    op: Literal["replace", "insert_after", "insert_before", "delete", "append"] = Field(
        description="replace 换掉一段；insert_after / insert_before 加在某段后面 / 前面；delete 删掉一段；append 加到页面最后")
    block: str | None = Field(None, description="段落编号，比如 k7qd（append 不用填）")
    markdown: str | None = Field(None, description="新的内容，可以是好几段（delete 不用填）")


@_tool("按段改资料库里的 markdown 页面：只动要改的段，不会冲掉别人刚改的地方。先用 read_page 读，拿到版本号和段落编号。"
       "一次可以改好几处，全部生效或者都不生效；要改的段在你读过之后被别人改了，会告诉你它现在的内容，按新版本号再提交就行。", read_only=False)
async def edit_page(page_id: int,
                    version: Annotated[int, Field(description="read_page 显示的版本号")],
                    ops: Annotated[list[EditOp], Field(description="按顺序执行的修改")],
                    ctx: Context) -> str:
    a = _agent(ctx, write=True)
    out, event = pagedoc.tool_edit(_by(a), {"page_id": page_id, "version": version,
                                            "ops": [o.model_dump(exclude_none=True) for o in ops]})
    if event:
        await registry.emit("page_changed", **event)
        await registry.emit("pages_changed")
    return out


@_tool("整页换成新的正文（markdown 页面）。没变的段保留原来的编号和作者，评论还挂着。要带 read_page 看到的版本号；"
       "会覆盖掉你读过之后别人改过的段时不写，告诉你那几段现在的内容。只改几处用 edit_page。", read_only=False)
async def rewrite_page(page_id: int,
                       version: Annotated[int, Field(description="read_page 显示的版本号")],
                       content: Annotated[str, Field(description="页面全文，Markdown（不要带段落编号）")],
                       ctx: Context,
                       title: Annotated[str | None, Field(description="可选：新标题，不给就不改")] = None) -> str:
    a = _agent(ctx, write=True)
    old = library.get_page(page_id, False)
    if not old:
        return f"没有页面 #{page_id}"
    if old["format"] != "markdown":
        return f"没保存：页面 #{page_id} 是 html 页面，只能读"
    if old["locked"]:
        return f"没保存：页面 #{page_id}「{old['title']}」被用户锁定了，只能读，有意见用 comment_page"
    try:
        p = library.save_page(None, title or old["title"], media.embed(content), "markdown", page_id, by=_by(a), base=version)
    except pagedoc.Conflict as e:
        return pagedoc.conflict_text(e, "没保存")
    except (ValueError, TypeError, LookupError) as e:
        return f"没保存：{e}"
    if p["version"] == old["version"]:
        return f"页面 #{page_id}「{p['title']}」没有变化（版本 {p['version']}）"
    await registry.emit("page_changed", page_id=p["id"], version=p["version"], by=_by(a), title=p["title"],
                        updated=p["updated"], changed=[])   # changed 为空：网页上开着的整页重新拉
    await registry.emit("pages_changed")
    return f"已保存页面 #{p['id']}「{p['title']}」，现在是版本 {p['version']}：{_url(ctx, p['id'])}"


@_tool("看一个页面上的评论：每串评论挂在哪一段（⟦段落编号⟧，没有就是对整页）、谁说的、回复。默认只看没解决的。", read_only=True)
async def list_comments(page_id: int, ctx: Context,
                        include_resolved: Annotated[bool, Field(description="也列出已经解决的")] = False) -> str:
    _agent(ctx)
    p = library.get_page(page_id, False)
    if not p:
        return f"没有页面 #{page_id}"
    text = pagecomments.render(page_id, 50, resolved=include_resolved)
    if text:
        return text
    if pagecomments.comments(page_id):
        return f"页面 #{page_id}「{p['title']}」上的评论都解决了（include_resolved=true 能看到）"
    return f"页面 #{page_id}「{p['title']}」上还没有评论"


@_tool("在资料库页面上写评论、回复评论、把一串评论标成已解决。处理完用户的评论后用它回复（reply_to 填那条评论的编号），"
       "事情办完了加 resolve=true。也能对某一段（block 填段落编号）或整页提意见。你写的评论不会叫醒 OSA 里的 Bot。", read_only=False)
async def comment_page(page_id: int, ctx: Context,
                       text: Annotated[str | None, Field(description="评论内容（只想标成已解决可以不填）")] = None,
                       block: Annotated[str | None, Field(description="可选：评论哪一段（read_page 里的段落编号）；不填就是整页")] = None,
                       reply_to: Annotated[int | None, Field(description="可选：回复哪条评论（#编号）")] = None,
                       resolve: Annotated[bool, Field(description="可选：把这串评论标成已解决（要和 reply_to 一起用）")] = False) -> str:
    a = _agent(ctx, write=True)
    out, pid = pagecomments.tool_comment(_by(a), {"page_id": page_id, "text": text, "block": block, "reply_to": reply_to,
                                                  "resolve": resolve})
    if pid:
        await registry.emit("page_comments", page_id=pid)
    return out


# ---------- /mcp 入口 ----------
async def _reply(send, status: int, detail: str, headers: list | None = None):
    body = json.dumps({"detail": detail}, ensure_ascii=False).encode()
    await send({"type": "http.response.start", "status": status,
                "headers": [(b"content-type", b"application/json; charset=utf-8"), (b"content-length", str(len(body)).encode()),
                            *(headers or [])]})
    await send({"type": "http.response.body", "body": body})


class Endpoint:
    """ASGI 应用（不是函数：Starlette 的 Route 才会原样转给它，不包成普通的请求处理）。"""

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return
        if security.demo():
            return await _reply(send, 404, i18n.t("演示服务器不开放外部 Agent"))
        auth = next((v.decode("latin-1") for k, v in scope["headers"] if k == b"authorization"), "")
        kind, _, token = auth.partition(" ")
        a = check(token.strip()) if kind.lower() == "bearer" else None
        if not a:
            return await _reply(send, 401, i18n.t("token 不对或者已经撤销了：在 设置 → 外部 Agent 里新建一个"),
                                [(b"www-authenticate", b'Bearer realm="osa"')])
        # 无状态：每个请求起一个会话管理、处理完就收掉，不留常驻的任务（不用进 lifespan，测试里的 ASGITransport 也不跑 lifespan）。
        # Host、来源 auth.py 已经查过，这里不再查（它默认只认 127.0.0.1）
        manager = StreamableHTTPSessionManager(app=server._lowlevel_server, stateless=True, json_response=True,
                                               security_settings=TransportSecuritySettings(enable_dns_rebinding_protection=False))
        async with manager.run():
            await manager.handle_request({**scope, "osa.agent": a}, receive, send)


endpoint = Endpoint()
