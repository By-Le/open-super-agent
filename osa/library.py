"""资料库：Bot 生成的页面（Pages），以及共用电脑工作文件夹里的文件（Docker / Instabox 是 /data/workspace，本机电脑见 host.py）。

页面存在数据库里，格式是 markdown 或 html。分享就是生成一个不可猜的链接 /s/<token>，取消分享后链接失效。
html 页面只通过带 CSP 的 /raw 地址在沙箱 iframe 里显示：脚本可以跑，但拿不到这个站点的任何数据，也连不上接口。
带清单 app 的 html 页面是小应用（见 applets.py）：页面里能通过 postMessage 调清单里允许的工具、读写自己的数据（page_data 表）。
文件直接读写电脑里的目录（Computer.ls / stat_paths / rm / read_bytes / write_bytes），Docker 和 Instabox 要先开机。
"""
import json
import posixpath
import re
import secrets
import time
from . import i18n
from .memory import conn

conn.execute("""create table if not exists pages(
  id integer primary key, bot_id integer, title text, format text default 'markdown', content text,
  share_token text, pinned integer default 0, task_id integer, created real, updated real, app text)""")
conn.execute("create unique index if not exists pages_token on pages(share_token)")
_have = {r[1] for r in conn.execute("pragma table_info(pages)")}
for _col, _type in (("app", "text"), ("block_meta", "text"), ("version", "integer default 0"), ("locked", "integer default 0"),
                    ("parent_id", "integer"), ("pos", "real"), ("agent_id", "integer"),
                    ("kind", "text default 'page'"), ("schema", "text"), ("views", "text"), ("props", "text"), ("props_meta", "text")):
    if _col not in _have:   # block_meta / version / locked：按段编辑（pagedoc.py）；parent_id / pos：子页面和它们的顺序；
        # agent_id：建它的外部 Agent（agentapi.py），这时 bot_id 是空的；
        # kind：page / db（数据库）/ row（数据库的一行）；schema / views 是库的表结构和视图，props / props_meta 是行的属性（见 dbs.py）
        conn.execute(f"alter table pages add column {_col} {_type}")
conn.execute("""create table if not exists page_data(
  page_id integer, key text, value text, updated real, primary key(page_id, key))""")
conn.execute("create table if not exists file_pins(path text primary key, ts real)")
conn.commit()

FORMATS = ("markdown", "html")
MAX_PAGE = 500_000
MAX_PAGES = 1000
LEGACY = "/data/workspace"   # Docker / Instabox 电脑里的工作文件夹
MAX_UPLOAD = 20 << 20
RAW_CSP = ("sandbox allow-scripts allow-popups; default-src 'none'; script-src 'unsafe-inline' https:; "
           "style-src 'unsafe-inline' https:; img-src https: data:; font-src https: data:; media-src https: data:; "
           "connect-src 'none'; frame-src 'none'; form-action 'none'")

_COLS = ("id, bot_id, title, format, share_token, pinned, task_id, created, updated, length(content), app, version, locked, parent_id, pos, "
         "agent_id, coalesce(kind, 'page')")
KINDS = ("page", "db", "row")


# ---------- 页面 ----------
def _page(r, content: str | None = None) -> dict:
    i, bot_id, title, fmt, token, pinned, task_id, created, updated, size, app, version, locked, parent_id, pos, agent_id, kind = r
    p = {"id": i, "bot_id": bot_id, "title": title, "format": fmt, "share_token": token, "pinned": bool(pinned),
         "task_id": task_id, "created": created, "updated": updated, "size": size or 0,
         "app": json.loads(app) if app else None, "version": version or 0, "locked": bool(locked), "parent_id": parent_id, "pos": pos or 0, "agent_id": agent_id, "kind": kind}
    if content is not None:
        p["content"] = content
    return p


def list_pages() -> list[dict]:
    return [_page(r) for r in conn.execute(f"select {_COLS} from pages order by pinned desc, updated desc")]


def get_page(pid: int, with_content: bool = True) -> dict | None:
    r = conn.execute(f"select {_COLS}, content from pages where id=?", (pid,)).fetchone()
    return _page(r[:-1], r[-1] if with_content else None) if r else None


def by_token(token: str) -> dict | None:
    if not token:
        return None
    r = conn.execute("select id from pages where share_token=?", (token,)).fetchone()
    return get_page(r[0]) if r else None


def save_page(bot_id: int | None, title: str, content: str, fmt: str = "markdown",
              pid: int | None = None, task_id: int | None = None, app=None, by: str | None = None,
              base: int | None = None, allow_empty: bool = False, parent_id: int | None = None,
              kind: str = "page", props: dict | None = None) -> dict:
    """新建，或者给 pid 时更新内容。app 是小应用的清单（见 applets.manifest），只对 html 有效；更新时不给就保留原来的。
    by 是谁改的（默认 bot_id 对应的 Bot，没有就是用户；agent:<id> 是外部 Agent，新建时记进 agent_id）。markdown 页面更新时按段对齐（pagedoc.rewrite），没变的段保留编号和作者；
    给了 base（调用方读到的版本号），会覆盖掉 base 之后别人改过的段就抛 pagedoc.Conflict。allow_empty：用户新建的空白页面。
    parent_id 只在新建时有用：放在哪个页面下面（排在最后）。kind、props 也只在新建时有用：数据库（db）和它的行（row），见 dbs.py；
    行的属性先写好再记第一版历史，第一版里就有属性。"""
    from . import applets, pagedoc
    title = " ".join((title or "").split())[:120] or ("" if kind == "row" else i18n.t("未命名页面"))   # 数据库的行可以没有名称
    fmt = fmt if fmt in FORMATS else "markdown"
    app = applets.manifest(app) if fmt == "html" else None
    by = by if by is not None else (f"bot:{bot_id}" if bot_id else "user")
    content = content or ""
    if not content.strip() and not allow_empty:
        raise ValueError(i18n.t("页面内容不能为空"))
    if len(content) > MAX_PAGE:
        raise ValueError(i18n.t("页面太大（最多 {n}K 字）", n=MAX_PAGE // 1000))
    if base is not None and (not isinstance(base, int) or isinstance(base, bool)):
        raise ValueError(i18n.t("版本号要是整数"))
    now = time.time()
    if pid is not None:
        if not (old := get_page(pid, False)):
            raise ValueError(i18n.t("没有页面 #{id}", id=pid))
        if app is None and fmt == "html":
            app = old["app"]
        if fmt == "markdown" and old["format"] == "markdown":
            pagedoc.rewrite(pid, title, content, by, base)
        else:   # html 页面，或者换了格式：整页换掉，段的编号以后按需重建
            conn.execute("update pages set format=?, app=?, block_meta=null where id=?",
                         (fmt, json.dumps(app, ensure_ascii=False) if app else None, pid))
            pagedoc.snapshot_html(pid, by, title=title, content=content)
    else:
        if kind not in KINDS:
            raise ValueError(f"kind 只能是 {' / '.join(KINDS)}")
        if kind != "row" and conn.execute("select count(*) from pages where coalesce(kind, 'page') != 'row'").fetchone()[0] >= MAX_PAGES:
            raise ValueError(i18n.t("资料库最多 {n} 个页面，先删掉一些", n=MAX_PAGES))   # 数据库的行另算（dbs.MAX_ROWS）
        if parent_id is not None or kind == "row":
            _check_parent(parent_id, kind=kind)
        agent_id = int(by[6:]) if by.startswith("agent:") and by[6:].isdigit() else None
        pid = conn.execute("insert into pages(bot_id, title, format, content, task_id, created, updated, app, parent_id, pos, agent_id, kind, props) "
                           "values(?,?,?,?,?,?,?,?,?,?,?,?,?)", (bot_id, title, fmt, content, task_id, now, now,
                                                                 json.dumps(app, ensure_ascii=False) if app else None,
                                                                 parent_id, _end_pos(parent_id), agent_id, kind,
                                                                 json.dumps(props, ensure_ascii=False) if props is not None else None)).lastrowid
        conn.commit()
        if fmt == "markdown":
            pagedoc.init(pid, by)
        else:
            pagedoc.snapshot_html(pid, by, created=True)
    conn.commit()
    return get_page(pid, False)


def update_page(pid: int, title: str | None = None, pinned: bool | None = None, locked: bool | None = None,
                by: str = "user") -> dict:
    """改标题（记一版历史）、置顶、锁定（锁定后 Bot 只能读，用户照样能改）。"""
    from . import pagedoc
    if not get_page(pid, False):
        raise ValueError(i18n.t("没有页面 #{id}", id=pid))
    if title is not None:
        pagedoc.set_title(pid, " ".join(title.split())[:120] or i18n.t("未命名页面"), by)
    if pinned is not None:
        conn.execute("update pages set pinned=? where id=?", (int(pinned), pid))
    if locked is not None:
        conn.execute("update pages set locked=? where id=?", (int(locked), pid))
    conn.commit()
    return get_page(pid, False)


def share(pid: int, on: bool) -> dict:
    """打开分享时生成新链接；关掉后旧链接立即失效。"""
    conn.execute("update pages set share_token=? where id=?", (secrets.token_urlsafe(16) if on else None, pid))
    conn.commit()
    return get_page(pid, False)


def delete_page(pid: int, cascade: bool = False, trash: bool = True):
    """删页面。子页面默认挪到上一级、排在它原来的位置；cascade 就连子页面一起删。数据库总是连行一起删（行不能离开库）。
    trash：先整个（连版本历史、评论、小应用的数据，库连同它的行）收进回收站，30 天内能恢复。"""
    from . import pagecomments, pagedoc   # noqa: F401  确保版本历史、评论的表已经建好
    p = get_page(pid, False)
    if not p:
        return
    kids = _children(pid)
    gone = [pid]
    if cascade or p["kind"] == "db":
        gone += sorted(descendants(pid), key=lambda i: (len(_ancestors_ids(i)), i))
    if trash:
        conn.execute("insert into page_trash(page_id, title, kind, format, deleted, data) values(?,?,?,?,?,?)",
                     (pid, p["title"], p["kind"], p["format"], time.time(), json.dumps({"pages": [_dump(i) for i in gone]}, ensure_ascii=False)))
    if not (cascade or p["kind"] == "db") and kids:
        after = _next_after(p["parent_id"], p["pos"])
        step = ((after if after is not None else p["pos"] + 1) - p["pos"]) / (len(kids) + 1)
        for n, (kid, _) in enumerate(kids, 1):
            conn.execute("update pages set parent_id=?, pos=? where id=?", (p["parent_id"], p["pos"] + step * n, kid))
    for i in gone:
        _purge(i)
    conn.commit()


def _purge(pid: int):
    from . import pagecomments
    conn.execute("delete from pages where id=?", (pid,))
    conn.execute("delete from page_data where page_id=?", (pid,))
    conn.execute("delete from page_versions where page_id=?", (pid,))
    pagecomments.drop_page(pid)


def _ancestors_ids(pid: int) -> list[int]:
    out, cur = [], pid
    while (r := conn.execute("select parent_id from pages where id=?", (cur,)).fetchone()) and r[0] is not None and r[0] not in out:
        out.append(r[0])
        cur = r[0]
    return out


def _rows(q: str, args) -> list[dict]:
    cur = conn.execute(q, args)
    cols = [d[0] for d in cur.description]
    return [dict(zip(cols, r)) for r in cur.fetchall()]


def _dump(pid: int) -> dict:
    """一个页面的全部：页面这一行、版本历史、评论、小应用的数据。"""
    return {"page": _rows("select * from pages where id=?", (pid,))[0],
            "versions": _rows("select * from page_versions where page_id=? order by id", (pid,)),
            "comments": _rows("select * from page_comments where page_id=? order by id", (pid,)),
            "data": _rows("select * from page_data where page_id=?", (pid,))}


# ---------- 回收站：删掉的页面留 30 天 ----------
TRASH_DAYS = 30
conn.execute("""create table if not exists page_trash(
  id integer primary key, page_id integer, title text, kind text, format text, deleted real, data text)""")
conn.commit()


def trash() -> list[dict]:
    out = []
    for i, pid, title, kind, fmt, deleted, n in conn.execute(
            "select id, page_id, title, kind, format, deleted, json_array_length(data, '$.pages') from page_trash order by deleted desc"):
        out.append({"id": i, "page_id": pid, "title": title, "kind": kind or "page", "format": fmt, "deleted": deleted,
                    "count": n or 1, "expires": deleted + TRASH_DAYS * 86400})
    return out


def _insert(table: str, row: dict, drop_id: bool = False) -> int:
    have = {r[1] for r in conn.execute(f"pragma table_info({table})")}
    row = {k: v for k, v in row.items() if k in have and not (drop_id and k == "id")}
    return conn.execute(f"insert into {table}({', '.join(row)}) values({', '.join('?' * len(row))})", list(row.values())).lastrowid


def restore_trash(tid: int) -> dict:
    """从回收站放回去：原来的上级还在就放回原来的位置，不在了放到最上层。编号被别的页面占了就换一个新的。"""
    from . import pagecomments, pagedoc   # noqa: F401  确保版本历史、评论的表已经建好
    r = conn.execute("select data, kind from page_trash where id=?", (tid,)).fetchone()
    if not r:
        raise LookupError(i18n.t("回收站里没有这一项"))
    pages = json.loads(r[0])["pages"]
    top = pages[0]["page"]
    if top.get("kind") == "row" and not conn.execute("select 1 from pages where id=? and kind='db'", (top.get("parent_id"),)).fetchone():
        raise ValueError(i18n.t("这一行所在的数据库已经不在了，恢复不了"))
    if top.get("kind") != "row" and conn.execute("select count(*) from pages where coalesce(kind, 'page') != 'row'").fetchone()[0] >= MAX_PAGES:
        raise ValueError(i18n.t("资料库最多 {n} 个页面，先删掉一些", n=MAX_PAGES))
    ids: dict[int, int] = {}
    for item in pages:
        pg = dict(item["page"])
        old = pg["id"]
        if conn.execute("select 1 from pages where id=?", (old,)).fetchone():
            pg.pop("id")
        if pg.get("parent_id") in ids:
            pg["parent_id"] = ids[pg["parent_id"]]
        elif item is pages[0] and pg.get("parent_id") is not None and not conn.execute("select 1 from pages where id=?", (pg["parent_id"],)).fetchone():
            pg["parent_id"] = None   # 原来的上级不在了：放到最上层的最后
            pg["pos"] = _end_pos(None)
        if pg.get("share_token") and conn.execute("select 1 from pages where share_token=?", (pg["share_token"],)).fetchone():
            pg["share_token"] = None
        ids[old] = _insert("pages", pg, drop_id="id" not in pg)
        for v in item["versions"]:
            _insert("page_versions", {**v, "page_id": ids[old]}, drop_id=True)
        cids: dict[int, int] = {}
        for c in item["comments"]:
            cids[c["id"]] = _insert("page_comments", {**c, "page_id": ids[old], "parent_id": cids.get(c.get("parent_id"), c.get("parent_id"))},
                                    drop_id=True)
        for d in item["data"]:
            _insert("page_data", {**d, "page_id": ids[old]})
    conn.execute("delete from page_trash where id=?", (tid,))
    conn.commit()
    return get_page(ids[top["id"]], False)


def drop_trash(tid: int | None = None):
    """彻底删掉回收站里的一项（不给就清空）。"""
    if tid is None:
        conn.execute("delete from page_trash")
    else:
        conn.execute("delete from page_trash where id=?", (tid,))
    conn.commit()


def purge_trash():
    """回收站里放了 30 天的清掉。"""
    conn.execute("delete from page_trash where deleted < ?", (time.time() - TRASH_DAYS * 86400,))
    conn.commit()


# ---------- 子页面 ----------
def _children(pid: int | None) -> list[tuple[int, float]]:
    q = "select id, coalesce(pos, 0) from pages where parent_id " + ("is null" if pid is None else "=?") + " order by coalesce(pos, 0), id"
    return list(conn.execute(q, () if pid is None else (pid,)))


def _end_pos(parent_id: int | None) -> float:
    kids = _children(parent_id)
    return (max(p for _, p in kids) + 1) if kids else 1.0


def _next_after(parent_id: int | None, pos: float) -> float | None:
    return next((p for _, p in _children(parent_id) if p > pos), None)


def descendants(pid: int) -> set[int]:
    out, todo = set(), [pid]
    while todo:
        for kid, _ in _children(todo.pop()):
            if kid not in out:
                out.add(kid)
                todo.append(kid)
    return out


def _check_parent(parent_id: int | None, pid: int | None = None, kind: str = "page"):
    """行只能在数据库下面；别的页面只能放在普通的 markdown 页面下面（数据库、行下面不放子页面）。"""
    parent = get_page(parent_id, False) if isinstance(parent_id, int) and not isinstance(parent_id, bool) else None
    if not parent:
        raise ValueError(i18n.t("没有页面 #{id}", id=parent_id))
    if kind == "row" or parent["kind"] != "page":
        if kind == "row" and parent["kind"] == "db":
            return
        raise ValueError(i18n.t("数据库的行只能在数据库里") if kind == "row" else i18n.t("数据库和它的行下面不能放子页面"))
    if parent["format"] != "markdown":
        raise ValueError(i18n.t("html 页面下面不能放子页面"))
    if pid is not None and (parent_id == pid or parent_id in descendants(pid)):
        raise ValueError(i18n.t("不能挪到它自己或者它的子页面下面"))


def move(pid: int, parent_id: int | None, after: int | None = None) -> dict:
    """挪到 parent_id 下面（None 是最上层），排在 after 后面（None 是排在最后，0 是排在最前）。
    数据库的行只能在自己的库里换顺序。"""
    p = get_page(pid, False)
    if not p:
        raise ValueError(i18n.t("没有页面 #{id}", id=pid))
    if p["kind"] == "row" and parent_id != p["parent_id"]:
        raise ValueError(i18n.t("数据库的行不能挪出数据库"))
    if parent_id is not None:
        _check_parent(parent_id, pid, p["kind"])
    kids = [(k, p) for k, p in _children(parent_id) if k != pid]
    if after is None:
        pos = (kids[-1][1] + 1) if kids else 1.0
    elif after == 0:
        pos = (kids[0][1] - 1) if kids else 1.0
    else:
        at = next((n for n, (k, _) in enumerate(kids) if k == after), None)
        if at is None:
            raise ValueError(i18n.t("页面 #{id} 不在那一层", id=after))
        pos = (kids[at][1] + kids[at + 1][1]) / 2 if at + 1 < len(kids) else kids[at][1] + 1
    conn.execute("update pages set parent_id=?, pos=? where id=?", (parent_id, pos, pid))
    conn.commit()
    return get_page(pid, False)


def family(pid: int) -> dict:
    """上级（从最上层往下）和子页面（按顺序），给页面顶上的路径和底下的子页面列表用。"""
    ancestors, seen = [], {pid}
    p = get_page(pid, False)
    while p and p["parent_id"] is not None and p["parent_id"] not in seen:
        seen.add(p["parent_id"])
        p = get_page(p["parent_id"], False)
        if p:
            ancestors.insert(0, {"id": p["id"], "title": p["title"]})
    kids = [get_page(k, False) for k, _ in _children(pid)] if (p0 := get_page(pid, False)) and p0["kind"] == "page" else []   # 库的行不算子页面
    return {"ancestors": ancestors, "children": [{"id": k["id"], "title": k["title"], "format": k["format"], "kind": k["kind"]} for k in kids if k]}


# ---------- 小应用的数据 ----------
def get_data(pid: int) -> dict:
    return {k: json.loads(v) for k, v in conn.execute("select key, value from page_data where page_id=? order by key", (pid,))}


def set_data(pid: int, updates: dict) -> dict:
    """合并写入：值为 None 的键删掉。返回全部数据。"""
    from . import applets
    cur = get_data(pid)
    new = applets.check_data(updates, cur)
    now = time.time()
    for k in set(cur) - set(new):
        conn.execute("delete from page_data where page_id=? and key=?", (pid, k))
    for k, v in new.items():
        if k not in cur or cur[k] != v:
            conn.execute("insert or replace into page_data(page_id, key, value, updated) values(?,?,?,?)",
                         (pid, k, json.dumps(v, ensure_ascii=False), now))
    conn.commit()
    return new


def link(pid: int) -> str:
    """对话里引用页面用的地址，前端会在资料库里打开它。"""
    return f"/library/pages/{pid}"


_LINK = re.compile(r"/library/pages/(\d+)")


def recent_pages(chat_id: int, limit: int = 3, scan: int = 30) -> str:
    """对话最近 scan 条消息里提到的页面（新的在前，至多 limit 个），每个一行：版本、没解决的评论、最后谁改的。
    交接时接手的 Bot 每轮都看得到页面现在的样子，不用记着去查。"""
    from . import pagedoc
    seen: list[int] = []
    for text, meta in conn.execute("select content, meta from messages where chat_id=? order by id desc limit ?", (chat_id, scan)):
        found = [int(m) for m in reversed(_LINK.findall(text or ""))]
        if meta and '"page"' in meta:   # 页面上的评论叫醒 Bot 时私聊里那条通知（kind=page_comment）
            try:
                if isinstance(pg := json.loads(meta).get("page"), int):
                    found.insert(0, pg)
            except ValueError:
                pass
        for pid in found:
            if pid not in seen:
                seen.append(pid)
        if len(seen) >= limit * 3:
            break
    out = []
    for pid in seen:
        p = get_page(pid, False)
        if not p:
            continue
        n = conn.execute("select count(*) from page_comments where page_id=? and parent_id is null and resolved=0", (pid,)).fetchone()[0]
        last = conn.execute("select by, ts from page_versions where page_id=? and label != 'base' order by id desc limit 1", (pid,)).fetchone()
        who = pagedoc.name_of(last[0]) if last and last[0] else ""
        ago = int((time.time() - (last[1] if last else p["updated"])) // 60)
        when = "刚刚" if ago < 1 else f"{ago} 分钟前" if ago < 60 else f"{ago // 60} 小时前" if ago < 1440 else f"{ago // 1440} 天前"
        out.append(f"- #{pid}《{p['title']}》版本 {p['version']}" + (f" · {n} 串评论没解决" if n else "")
                   + (f" · 最后是 {who} 改的（{when}）" if who else f" · {when}更新") + (" · 已锁定" if p["locked"] else ""))
        if len(out) >= limit:
            break
    return "\n".join(out)


SAVE_PAGE = {"type": "function", "function": {
    "name": "save_page",
    "description": "把成品（报告、清单、对比表、网页等）保存成资料库里的页面，返回页面链接。"
                   "用户要“做成页面/报告/可以分享的页面”，或结果很长、以后还要看时用。给 page_id 就是整页更新那个页面"
                   "（markdown 页面别的 Bot 建的也能更新，要带上 read_page 显示的 version；只改几处用 edit_page）。"
                   "markdown 里放对话里的图片（上下文里“[图片]”后面的编号，包括你画的）写成 ![说明](/api/media/编号)。"
                   "要做成可交互的小应用（表单、看板、计算器、能点按钮查东西的面板）时 format=html 并填 app，"
                   "页面里用 window.osa 调工具、读写数据、给你发消息（见系统提示词里的说明）。",
    "parameters": {"type": "object", "properties": {
        "title": {"type": "string"},
        "content": {"type": "string", "description": "页面全文。markdown 用 Markdown；html 是完整的 HTML（可以带内联 CSS 和脚本）"},
        "format": {"type": "string", "enum": list(FORMATS)},
        "page_id": {"type": "integer", "description": "可选：要更新的页面编号"},
        "version": {"type": "integer", "description": "更新时：read_page 显示的版本号（页面最近是别人改的就必须带）"},
        "parent_id": {"type": "integer", "description": "可选，新建时：放在哪个页面下面当子页面"},
        "app": {"type": "object", "description": "可选，只对 html 有效：填了就是小应用。tools 是页面里允许调用的工具名（只能是你现在有的工具）；"
                                                 "chat 为 true 时页面能替用户给你发消息",
                "properties": {"tools": {"type": "array", "items": {"type": "string"}}, "chat": {"type": "boolean"}}}},
        "required": ["title", "content"]}}}


PAGE_DATA = {"type": "function", "function": {
    "name": "page_data",
    "description": "读或写一个小应用的数据（键值对）。用户在小应用里填的内容在这里；你写进去的数据小应用里马上能看到（osa.store / osa.on('data')）。"
                   "不给 set 就只读。",
    "parameters": {"type": "object", "properties": {
        "page_id": {"type": "integer"},
        "set": {"type": "object", "description": "要写入的键值（值可以是任何 JSON；设为 null 删除这个键）"}},
        "required": ["page_id"]}}}


async def tool_page_data(bot_id: int, args: dict) -> str:
    pid = args.get("page_id")
    p = get_page(pid, False) if isinstance(pid, int) else None
    if not p:
        return f"没有页面 #{pid}"
    if not p.get("app"):
        return f"页面 #{pid} 不是小应用，没有数据。要做成小应用，用 save_page 更新它并填 app"
    if (upd := args.get("set")) is not None:
        if p["bot_id"] != bot_id:
            return f"没写：小应用 #{pid} 不是你建的，只能读"
        try:
            data = set_data(pid, upd)
        except (ValueError, TypeError) as e:
            return f"没写：{e}"
        from . import registry
        await registry.emit("page_data", page_id=pid, data=data)
        head = f"已写入 {len(upd)} 个键。"
    else:
        data, head = get_data(pid), ""
    text = json.dumps(data, ensure_ascii=False, indent=1)
    return f"{head}小应用「{p['title']}」的数据（{len(data)} 个键）：\n{text[:12000]}" + ("\n…（太长，截断）" if len(text) > 12000 else "")



READ_PAGE = {"type": "function", "function": {
    "name": "read_page",
    "description": "读资料库里一个页面（html 页面只给文字）。markdown 页面带版本号，每段前面有段落编号，改的时候给 edit_page 用。"
                   "页面编号可以用 search_memory 查到",
    "parameters": {"type": "object", "properties": {
        "page_id": {"type": "integer"},
        "start": {"type": "string", "description": "可选：从这个段落编号开始读（页面太长、上次没读完时）"}},
        "required": ["page_id"]}}}


def tool_read_page(args: dict, limit: int = 12000) -> str:
    from . import pagedoc, search
    p = get_page(args.get("page_id")) if isinstance(args.get("page_id"), int) else None
    if not p:
        return f"没有页面 #{args.get('page_id')}"
    if p["format"] == "markdown":
        return pagedoc.render(p["id"], args.get("start"), limit)
    text = search._plain(p["content"] or "", p["format"])
    if p["format"] == "html":
        text = re.sub(r"[ \t]+", " ", re.sub(r"\n\s*\n+", "\n\n", text)).strip()
    more = f"\n…（还有 {len(text) - limit} 字没显示）" if len(text) > limit else ""
    return f"《{p['title']}》{link(p['id'])}，{time.strftime('%Y-%m-%d', time.localtime(p['updated']))} 更新\n\n{text[:limit]}{more}"


def tool_save_page(bot_id: int, args: dict, task_id: int | None = None, saved: list | None = None) -> str:
    """saved：传一个列表进来，保存成功的页面会放进去（调用方据此发页面改动的事件）。"""
    from . import pagedoc
    pid = args.get("page_id")
    fmt = args.get("format")
    if pid is not None:
        if not (old := get_page(pid, False)):
            return f"没保存：页面 #{pid} 不存在"
        fmt = fmt or old["format"]
        if old["kind"] == "db":
            return f"没保存：#{pid} 是数据库，加行用 db_add、改属性用 db_update、改表结构用 db_schema"
        if "html" in (fmt, old["format"]):
            if old["bot_id"] != bot_id:
                return f"没保存：页面 #{pid} 是 html 页面 / 小应用，只有建它的 Bot 能更新"
        elif old["locked"] or pagedoc._db_locked(old):
            return f"没保存：页面 #{pid}「{old['title']}」被用户锁定了，只能读"
        elif args.get("version") is None and (who := pagedoc.last_by(pid)) != f"bot:{bot_id}":
            return (f"没保存：页面 #{pid} 最近是「{pagedoc.name_of(who)}」改的。先用 read_page 看现在的内容，"
                    f"再带上 version 更新；只改几处用 edit_page 更省事")
    app = args.get("app")
    if app and (unknown := [t for t in (app.get("tools") or []) if isinstance(t, str) and not applets_known(t)]):
        return f"没保存：app.tools 里有现在没有的工具 {', '.join(unknown)}，只能列你这一轮能用的工具"
    try:
        from . import media
        content = args.get("content", "")
        if (fmt or "markdown") == "markdown" and isinstance(content, str):
            content = media.embed(content)
        p = save_page(bot_id, args.get("title", ""), content, fmt or "markdown",
                      pid, task_id, app, by=f"bot:{bot_id}", base=args.get("version"),
                      parent_id=args.get("parent_id") if pid is None else None)
    except pagedoc.Conflict as e:
        return pagedoc.conflict_text(e, "没保存")
    except (ValueError, TypeError) as e:
        return f"没保存：{e}"
    if saved is not None:
        saved.append(p)
    if p["app"]:
        return (f"已保存小应用 #{p['id']}「{p['title']}」，链接 {link(p['id'])}。回复里用 Markdown 链接附上它，对话里会直接显示这个小应用，"
                f"用户能在里面操作。用 page_data 能读写它的数据（page_id={p['id']}）。")
    return (f"已保存页面 #{p['id']}「{p['title']}」（版本 {p['version']}），链接 {link(p['id'])}（用户在资料库里也能看到）。"
            "回复里用 Markdown 链接附上它。")


def applets_known(tool: str) -> bool:
    from . import applets
    return applets.known(tool)


# ---------- 文件 ----------
def root() -> str:
    """现在这台电脑的工作文件夹。"""
    from . import computer
    return computer.get().workspace


def norm(path: str | None, base: str | None = None) -> str:
    """把路径规范成工作文件夹下的绝对路径（相对路径从工作文件夹算起）；跑出去就报错。"""
    base = base or root()
    p = (path or "").strip() or base
    if base != LEGACY and (p == LEGACY or p.startswith(LEGACY + "/")):   # 技能、记忆里可能还写着 Docker 电脑里的路径
        p = base + p[len(LEGACY):]
    if not p.startswith("/"):
        p = posixpath.join(base, p)
    p = posixpath.normpath(p)
    if p != base and not p.startswith(base.rstrip("/") + "/"):
        raise ValueError(i18n.t("只能访问 {dir} 里的文件", dir=base))
    return p


def check_name(name: str) -> str:
    name = (name or "").strip()
    if not name or name in (".", "..") or "/" in name or "\0" in name or len(name) > 200:
        raise ValueError(i18n.t("文件名不合法"))
    return name


async def list_dir(c, path: str) -> list[dict]:
    out = await c.ls(norm(path, c.workspace))
    out.sort(key=lambda e: (not e["dir"], e["name"].lower()))
    return out


async def put_file(c, directory: str, name: str, data: bytes) -> dict:
    """把文件写进电脑里的目录。同名时自动加序号，不覆盖。"""
    d, name = norm(directory, c.workspace), check_name(name)
    if len(data) > MAX_UPLOAD:
        raise ValueError(i18n.t("文件太大（最多 {n} MB）", n=MAX_UPLOAD >> 20))
    try:
        existing = {e["name"] for e in await list_dir(c, d)}
    except ValueError:
        existing = set()   # 目录还不存在，写文件时会自动建
    stem, ext = posixpath.splitext(name)
    final, n = name, 1
    while final in existing:
        n += 1
        final = f"{stem} ({n}){ext}"
    path = posixpath.join(d, final)
    await c.write_bytes(path, data)
    return {"path": path, "name": final, "size": len(data)}


async def stat_many(c, paths: list[str]) -> dict[str, dict]:
    """置顶文件的现状（可能已经被删了）。"""
    return await c.stat_paths(paths) if paths else {}


async def remove(c, path: str):
    p = norm(path, c.workspace)
    if p == c.workspace:
        raise ValueError(i18n.t("不能删除整个工作目录"))
    await c.rm(p)
    unpin(p)


def pins() -> list[str]:
    """置顶的文件。换了电脑后，别的电脑里的置顶先不显示，换回去还在。"""
    base = root().rstrip("/") + "/"
    return [r[0] for r in conn.execute("select path from file_pins order by ts") if (r[0] + "/").startswith(base)]


def pin(path: str):
    conn.execute("insert or replace into file_pins values(?, ?)", (norm(path), time.time()))
    conn.commit()


def unpin(path: str):
    p = norm(path)
    conn.execute("delete from file_pins where path=? or path like ?", (p, p + "/%"))
    conn.commit()
