"""资料库页面的评论：用户在某一段上写评论，叫醒负责的 Bot 来处理（改页面、在评论里回复）。

- 评论挂在段上（block_id，见 pagedoc.py）；不挂段的是对整页的评论。回复挂在第一条下面（parent_id），一串评论一起解决、一起重新打开。
- 只有用户在网页上写的评论会叫醒 Bot。Bot 写的评论不叫醒任何 Bot，免得 Bot 之间来回停不下来；要找别的 Bot 用 message_bot / handoff 带上页面链接。
- 叫谁：明确指定的（wake）> 评论里 @ 到的（最多 3 个）> 回复的话是这串里最后说话的 Bot > 这一段的作者 > 建页面的 Bot > 主 Bot。
- 通知进 Bot 的私聊（kind=page_comment，memory.context 按存下的 meta 渲染，不现查页面：历史稳定，提示词缓存才命中），
  然后叫醒它说一轮（dm.wake，kind=comment，from 是这串评论的第一条：同一串连发几条只说一轮）。
- 那一轮说完，Bot 没在这串评论里回过，就把它这一轮的回复贴进去；那一轮派出去的任务做完，结果也贴回来（after_turn / after_task）。
"""
import asyncio
import json
import time
from . import blocks, bots, dm, i18n, library, pagedoc, registry
from .memory import conn

conn.execute("""create table if not exists page_comments(
  id integer primary key, page_id integer, block_id text, parent_id integer, by text, text text, quote text,
  ts real, resolved integer default 0, edited real, woke text)""")
conn.execute("create index if not exists page_comments_page on page_comments(page_id, id)")
conn.commit()

MAX_COMMENT = 4000
MAX_WAKE = 3
_COLS = "id, page_id, block_id, parent_id, by, text, quote, ts, resolved, edited, woke"


def _row(r) -> dict:
    i, pid, block, parent, by, text, quote, ts, resolved, edited, woke = r
    return {"id": i, "page_id": pid, "block_id": block, "parent_id": parent, "by": by, "text": text, "quote": quote or "",
            "ts": ts, "resolved": bool(resolved), "edited": edited, "woke": json.loads(woke) if woke else []}


def get(cid: int) -> dict | None:
    r = conn.execute(f"select {_COLS} from page_comments where id=?", (cid,)).fetchone()
    return _row(r) if r else None


def comments(pid: int, open_only: bool = False) -> list[dict]:
    out = [_row(r) for r in conn.execute(f"select {_COLS} from page_comments where page_id=? order by id", (pid,))]
    if open_only:
        live = {c["id"] for c in out if c["parent_id"] is None and not c["resolved"]}
        out = [c for c in out if (c["parent_id"] or c["id"]) in live]
    return out


def add(pid: int, by: str, text: str, block: str | None = None, quote: str | None = None, reply_to: int | None = None) -> dict:
    text = (text or "").strip()
    if not text:
        raise ValueError(i18n.t("评论不能是空的"))
    if len(text) > MAX_COMMENT:
        raise ValueError(i18n.t("评论太长（最多 {n} 字）", n=MAX_COMMENT))
    p, blks = pagedoc.load(pid)
    if not p:
        raise LookupError(i18n.t("没有页面 #{id}", id=pid))
    parent = None
    if reply_to is not None:
        root = get(reply_to) if isinstance(reply_to, int) else None
        if not root or root["page_id"] != pid:
            raise ValueError(i18n.t("没有评论 #{id}", id=reply_to))
        parent = root["parent_id"] or root["id"]
        block = get(parent)["block_id"]   # 回复跟着这串评论挂的段
        conn.execute("update page_comments set resolved=0 where id=?", (parent,))   # 解决过的又有人说话：重新打开
    elif block:
        block = blocks.clean_id(block)
        if block not in {b["id"] for b in blks}:
            raise ValueError(i18n.t("没有段落 ⟦{id}⟧", id=block))
    cid = conn.execute("insert into page_comments(page_id, block_id, parent_id, by, text, quote, ts) values(?,?,?,?,?,?,?)",
                       (pid, block or None, parent, by, text, (quote or "")[:500], time.time())).lastrowid
    conn.commit()
    return get(cid)


def update(pid: int, cid: int, text: str | None = None, resolved: bool | None = None) -> dict:
    c = get(cid)
    if not c or c["page_id"] != pid:
        raise LookupError(i18n.t("没有评论 #{id}", id=cid))
    if text is not None:
        text = text.strip()
        if not text or len(text) > MAX_COMMENT:
            raise ValueError(i18n.t("评论不能是空的") if not text else i18n.t("评论太长（最多 {n} 字）", n=MAX_COMMENT))
        conn.execute("update page_comments set text=?, edited=? where id=?", (text, time.time(), cid))
    if resolved is not None:
        conn.execute("update page_comments set resolved=? where id=?", (int(bool(resolved)), c["parent_id"] or cid))
    conn.commit()
    return get(cid)


def delete(pid: int, cid: int):
    """删一条；删的是一串的第一条就整串删掉。"""
    c = get(cid)
    if not c or c["page_id"] != pid:
        raise LookupError(i18n.t("没有评论 #{id}", id=cid))
    conn.execute("delete from page_comments where id=? or parent_id=?", (cid, cid if c["parent_id"] is None else -1))
    conn.commit()


def reanchor(pid: int, merged: list[list[str]]):
    """几段拼成了一段（见 pagedoc._commit）：挂在被并掉的段上的评论挪到留下的那段。"""
    for keep, *gone in merged:
        if gone:
            conn.execute(f"update page_comments set block_id=? where page_id=? and block_id in ({','.join('?' * len(gone))})",
                         (keep, pid, *gone))


def drop_page(pid: int):
    conn.execute("delete from page_comments where page_id=?", (pid,))


# ---------- 叫醒 Bot ----------
def _bot_of(by: str) -> int | None:
    return int(by[4:]) if by.startswith("bot:") and by[4:].isdigit() else None


def targets(page: dict, block: dict | None, root: int | None, text: str, wake) -> list[int]:
    everyone = bots.list_bots()
    alive = [b["id"] for b in everyone]
    if wake is False:
        return []
    if isinstance(wake, int) and not isinstance(wake, bool):
        return [wake] if wake in alive else []
    from . import groups
    if named := groups.mentions(text, alive):
        return named[:MAX_WAKE]
    if root:
        said = [_bot_of(c["by"]) for c in comments(page["id"]) if (c["parent_id"] or c["id"]) == root]
        if last := next((b for b in reversed(said) if b in alive), None):
            return [last]
    if block and (author := _bot_of(block.get("by", ""))) in alive:
        return [author]
    if page.get("bot_id") in alive:
        return [page["bot_id"]]
    return [b["id"] for b in everyone if b["is_primary"]][:1]   # 最后找主 Bot（没有就谁也不叫）


async def post(pid: int, text: str, block: str | None = None, quote: str | None = None, reply_to: int | None = None, wake=None) -> dict:
    """用户写评论：存下来，叫醒该处理的 Bot。返回这条评论（woke 里是叫醒了谁）。"""
    c = add(pid, "user", text, block, quote, reply_to)
    p, blks = pagedoc.load(pid)
    blk = next((b for b in blks if b["id"] == c["block_id"]), None)
    root = c["parent_id"] or c["id"]
    who = targets(p, blk, c["parent_id"], c["text"], wake)
    if who:
        conn.execute("update page_comments set woke=? where id=?", (json.dumps(who), c["id"]))
        conn.commit()
        c["woke"] = who
    quote = (c["quote"] or (blk["md"] if blk else ""))[:300]
    for bid in who:
        b = bots.get(bid)
        if not b:
            continue
        await dm._message(b["chat_id"], c["text"], bid, kind="page_comment", page=pid, title=p["title"], comment=c["id"],
                          root=root, block=c["block_id"], quote=quote)
        asyncio.create_task(dm.wake(bid, b["chat_id"], {"reply": None, "depth": 0, "woke": {
            "kind": "comment", "from": root, "page": pid, "title": p["title"], "comment": c["id"], "ts": c["ts"], "text": c["text"]}}))
    await registry.emit("page_comments", page_id=pid)
    return c


def _replied(pid: int, root: int, bot_id: int, since: float) -> bool:
    return any(c["by"] == f"bot:{bot_id}" and c["ts"] >= since for c in comments(pid) if (c["parent_id"] or c["id"]) == root)


async def after_turn(bot_id: int, woke: dict, shown: str, since: float):
    """因评论说的那一轮结束了：这一轮（since 开始）Bot 没在这串评论里回过，就把这一轮的回复贴进去（像私信的回信一样，不靠模型记得）。
    从这一轮开始算，不从评论的时间算：上一轮还没说完时又来一条评论，上一轮的回复不能算成这一轮的。"""
    pid, root = woke.get("page"), woke.get("from")
    if not shown.strip() or not get(root) or _replied(pid, root, bot_id, since):
        return
    try:
        add(pid, f"bot:{bot_id}", shown[:MAX_COMMENT], reply_to=root)
    except (ValueError, LookupError):
        return
    await registry.emit("page_comments", page_id=pid)


async def after_task(task):
    """评论那一轮派出去的任务做完了：Bot 还没在这串评论里回过（任务开始以后），把结果贴进去。"""
    pid, root = task.comment.get("page"), task.comment.get("root")
    if not get(root) or _replied(pid, root, task.bot_id, task.started):
        return
    text = task.result if task.status == "done" else i18n.t("（任务 #{id} 没做完：{result}）", id=task.id, result=task.result[:300])
    try:
        add(pid, f"bot:{task.bot_id}", text[:MAX_COMMENT], reply_to=root)
    except (ValueError, LookupError):
        return
    await registry.emit("page_comments", page_id=pid)


# ---------- 工具：comment_page ----------
COMMENT_PAGE = {"type": "function", "function": {
    "name": "comment_page",
    "description": "在资料库页面上写评论、回复评论、把一串评论标成已解决。用户在页面上给你写了评论时，处理完用它回复（reply_to 填那条评论的编号），"
                   "事情办完了加 resolve=true。也能对某一段（block 填段落编号）或整页提意见。Bot 写的评论不会叫醒别的 Bot。",
    "parameters": {"type": "object", "properties": {
        "page_id": {"type": "integer"},
        "text": {"type": "string", "description": "评论内容（只想标成已解决可以不填）"},
        "block": {"type": "string", "description": "可选：评论哪一段（read_page 里的段落编号）；不填就是整页"},
        "reply_to": {"type": "integer", "description": "可选：回复哪条评论（#编号）"},
        "resolve": {"type": "boolean", "description": "可选：把这串评论标成已解决（要和 reply_to 一起用）"}},
        "required": ["page_id"]}}}


def tool_comment(who: int | str, args: dict) -> tuple[str, int | None]:
    """comment_page：返回 (给模型的结果, 改了评论的页面编号)。who 是 Bot 的编号，或者作者（agent:<id>，外部 Agent）。
    都走 add，不叫醒任何 Bot。"""
    by = who if isinstance(who, str) else f"bot:{who}"
    pid = args.get("page_id")
    p = library.get_page(pid, False) if isinstance(pid, int) else None
    if not p:
        return f"没有页面 #{pid}", None
    text, reply_to = str(args.get("text") or "").strip(), args.get("reply_to")
    if reply_to is not None and not isinstance(reply_to, int):
        reply_to = int(str(reply_to).lstrip("#")) if str(reply_to).lstrip("#").isdigit() else -1
    if not text and not args.get("resolve"):
        return "没写：评论内容是空的", None
    try:
        out = []
        if text:
            c = add(pid, by, text, args.get("block"), None, reply_to)
            out.append(f"已在页面 #{pid}「{p['title']}」{'回复评论 #' + str(c['parent_id']) if c['parent_id'] else '写了评论'}（#{c['id']}）")
        if args.get("resolve"):
            if reply_to is None:
                return "没标：resolve 要和 reply_to 一起用，说明是哪串评论", None
            update(pid, reply_to, resolved=True)
            out.append(f"评论 #{reply_to} 那一串已标成已解决")
    except (ValueError, LookupError) as e:
        return f"没写：{e}", None
    return "；".join(out), pid


def render(pid: int, limit_threads: int = 20, resolved: bool = False) -> str:
    """read_page 末尾附上的未处理评论（每条截 300 字，最多 limit_threads 串）。resolved：已经解决的也列出来（外部 Agent 的 list_comments）。"""
    open_ = comments(pid, open_only=not resolved)
    roots = [c for c in open_ if c["parent_id"] is None]
    if not roots:
        return ""
    lines = [f"评论（{len(roots)} 串）：" if resolved else f"未处理的评论（{len(roots)}）："]
    for r in roots[:limit_threads]:
        where = f"⟦{r['block_id']}⟧ " if r["block_id"] else "整页 "
        lines.append(f"#{r['id']} {where}{'（已解决）' if r['resolved'] else ''}{pagedoc.name_of(r['by'])}：{r['text'][:300]}")
        lines += [f"  #{c['id']} {pagedoc.name_of(c['by'])}：{c['text'][:300]}" for c in open_ if c["parent_id"] == r["id"]]
    if len(roots) > limit_threads:
        lines.append(f"…还有 {len(roots) - limit_threads} 串没显示")
    return "\n".join(lines)
