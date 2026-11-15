"""从 Notion 导入资料库：用应用市场里连着的 Notion（官方托管的 MCP，用户已经登录授权）读页面和数据库，
在资料库里建成同样层级的页面和数据库。

- 只调读的工具（READ_TOOLS），不往 Notion 写。用服务器进程里现成的会话（mcp_hub.session_of），
  不另起连接：另起一个会刷新 OAuth 令牌，服务器手里的那个就失效了。
- 从选的根页面往下爬：页面正文里的 <page> / <database> 是子页面、子数据库。“View of …”这种链到同一份数据的库只建一次，
  别处的当成链接。
- 页面正文是 Notion 的 Markdown 加一些标签（<b>、<mention>、<callout>……），转成资料库认得的 Markdown（to_markdown）；
  页面之间的链接、提到的页面换成资料库里的地址。
- 数据库：属性按类型对过来（状态的分组、选项的颜色照搬；自动编号保留原来的号），每一行连正文一起导入；
  Notion 的人员、公式、汇总这些对不上的存成文字。文件属性里 Notion 存的文件下载下来存进资料库。
- 记着 Notion 的编号和资料库的编号（notion_map），再导一次是更新：页面正文按段对齐（没变的段编号、作者不变），行的属性改成 Notion 现在的。
"""
import asyncio
import json
import re
import time
import unicodedata

from . import dbs, i18n, library, mcp_hub, media
from .memory import conn

APP = "notion"
READ_TOOLS = {"notion-search", "notion-fetch", "notion-query-data-sources", "notion-get-users", "notion-get-self",
              "notion-get-comments", "notion-list-shared-pages", "notion-list-recent-pages"}
TIMEOUT = 60
MAX_NODES = 600        # 一次最多导多少页面和库（不算行）
PAGE_ROWS = 100        # 查行时一次取多少

conn.execute("""create table if not exists notion_map(
  nid text primary key, page_id integer, kind text, synced real)""")
conn.commit()


class NotConnected(RuntimeError):
    pass


async def call(tool: str, args: dict, timeout: float = TIMEOUT) -> str:
    """调一个只读的 Notion 工具，返回它给的文字。报错抛 RuntimeError（说清楚是 Notion 那边的）。"""
    if tool not in READ_TOOLS:
        raise ValueError(i18n.t("只能用 Notion 读的工具"))
    session = mcp_hub.session_of(APP)
    if session is None:
        raise NotConnected(i18n.t("Notion 没连上：先到应用市场连上 Notion"))
    res = await asyncio.wait_for(session.call_tool(tool, args), timeout)
    text = "\n".join(t for c in res.content if (t := getattr(c, "text", None)))
    if getattr(res, "is_error", None) or getattr(res, "isError", None):
        raise RuntimeError(i18n.t("Notion 报错：{error}", error=text[:300]))
    return text


# ---------- 读 Notion 给的文字 ----------
_HEX = re.compile(r"[0-9a-f]{32}")


def nid(s) -> str | None:
    """Notion 的编号（32 位十六进制，不带横线）：从网址、带横线的编号里取。"""
    s = str(s or "").lower().replace("-", "")
    m = _HEX.findall(s)
    return m[-1] if m else None


def _between(text: str, tag: str) -> str | None:
    m = re.search(rf"<{tag}>\s*(.*?)\s*</{tag}>", text, re.S)
    return m.group(1) if m else None


def parse_fetch(raw: str) -> dict:
    """notion-fetch 的结果 → {type: page | database, id, title, url, icon, props, content, sources: [{url, schema}], views}。"""
    try:
        j = json.loads(raw)
    except ValueError:
        j = {"text": raw}
    text = j.get("text") or ""
    kind = (j.get("metadata") or {}).get("type") or ("database" if text.lstrip().startswith("<database") else "page")
    out = {"type": kind, "title": (j.get("title") or "").strip(), "url": j.get("url") or "", "id": nid(j.get("url")),
           "props": {}, "content": "", "sources": [], "views": []}
    if kind == "page":
        try:
            out["props"] = json.loads(_between(text, "properties") or "{}")
        except ValueError:
            pass
        content = text.split("<content>", 1)[1] if "<content>" in text else ""
        out["content"] = content.rsplit("</content>", 1)[0].strip("\n")
        # 标题：页面的图标放在前面；标题自己已经以这个图标开头的不再加（不然成了“🗄 🗄 归档区”）
        icon = (re.search(r'<page\s+url="[^"]*"\s+icon="([^"]*)"', text) or [None, ""])[1]
        base = str(out["props"].get("title") or "").strip()
        if base:
            out["title"] = base if not icon or base.startswith(icon) else f"{icon} {base}"
    else:
        if not out["title"] and (m := re.search(r"The title of this Database is: (.*)", text)):
            out["title"] = m.group(1).strip()
        for m in re.finditer(r'<data-source url="\{*(collection://[^"}]+)\}*">(.*?)</data-source>', text, re.S):
            state = _between(m.group(2), "data-source-state")
            try:
                st = json.loads(state) if state else {}
            except ValueError:
                st = {}
            out["sources"].append({"url": m.group(1), "schema": st.get("schema") or {}})
        for m in re.finditer(r"<view url=\"\{*([^\"}]*)\}*\">\s*(\{.*?\})\s*</view>", text, re.S):
            try:
                out["views"].append({**json.loads(m.group(2)), "url": m.group(1)})
            except ValueError:
                pass
    return out


def children(content: str) -> list[tuple[str, str, str]]:
    """正文里的子页面、子数据库：[(page | database, Notion 编号, 标题)]，按出现的顺序。"""
    out = []
    for m in re.finditer(r'<(page|database)\s+url="\{*([^"}]+)\}*"[^>]*>\s*(.*?)\s*</\1>', content, re.S):
        if i := nid(m.group(2)):
            out.append((m.group(1), i, m.group(3).strip()))
    return out


# ---------- Notion 的 Markdown → 资料库的 Markdown ----------
def _punct(ch: str) -> bool:
    return bool(ch) and unicodedata.category(ch)[0] in "PS"


def _wrap(mark: str):
    """<b>x</b> → **x**：两头的空白挪到外面；CommonMark 认不出（比如“「x」**的”）时在外面补个空格。"""
    def sub(m: re.Match) -> str:
        before, inner, after_ = m.string[m.start() - 1:m.start()] if m.start() else "", m.group(1), m.string[m.end():m.end() + 1]
        lead, core, tail = re.match(r"(\s*)(.*?)(\s*)$", inner, re.S).groups()
        if not core:
            return inner
        pre = " " if not lead and _punct(core[0]) and before and not before.isspace() and not _punct(before) else ""
        post = " " if not tail and _punct(core[-1]) and after_ and not after_.isspace() and not _punct(after_) else ""
        return f"{pre}{lead}{mark}{core}{mark}{tail}{post}"
    return sub


_CALLOUT = {"⚠️": "WARNING", "⚠": "WARNING", "💡": "TIP", "❗": "IMPORTANT", "❗️": "IMPORTANT", "🚨": "CAUTION", "⛔": "CAUTION", "🛑": "CAUTION"}


def _callout_kind(icon: str, color: str) -> str:
    if icon in _CALLOUT:
        return _CALLOUT[icon]
    color = color or ""
    return "CAUTION" if color.startswith("red") else "WARNING" if color.startswith(("yellow", "orange")) else \
        "TIP" if color.startswith("green") else "NOTE"


def _table(html: str) -> str:
    """Notion 的 <table> → Markdown 表格（每格只留文字）。"""
    rows = []
    for tr in re.findall(r"<tr[^>]*>(.*?)</tr>", html, re.S):
        cells = [re.sub(r"\s*\n\s*", "<br>", c.strip()).replace("|", "\\|") for c in re.findall(r"<t[dh][^>]*>(.*?)</t[dh]>", tr, re.S)]
        rows.append(cells)
    if not rows:
        return ""
    n = max(len(r) for r in rows)
    rows = [r + [""] * (n - len(r)) for r in rows]
    head = rows[0] if 'header-row="true"' in html.split(">", 1)[0] else [""] * n
    body = rows[1:] if head is rows[0] else rows
    return "\n".join(["| " + " | ".join(head) + " |", "|" + " --- |" * n] + ["| " + " | ".join(r) + " |" for r in body])


def to_markdown(content: str, link=lambda url: None) -> str:
    """Notion 正文 → Markdown。link(网址) 给资料库里的地址（导入了的页面），None 就留原来的网址。"""
    keep: list[str] = []

    def protect(s: str) -> str:
        keep.append(s)
        return f"\x00{len(keep) - 1}\x00"
    text = content.replace("\r\n", "\n")
    text = re.sub(r"^(```.*?\n.*?\n```)[ \t]*$", lambda m: protect(m.group(1)), text, flags=re.S | re.M)   # 代码块原样

    def code(m: re.Match) -> str:
        s = m.group(1)
        fence = "``" if "`" in s else "`"
        pad = " " if s.startswith("`") or s.endswith("`") else ""
        return protect(f"{fence}{pad}{s}{pad}{fence}")
    text = re.sub(r"<code>(.*?)</code>", code, text)   # 行内代码里的 < > 是原文

    def href(url: str) -> str:
        return link(url) or url
    # 块
    text = re.sub(r'<(page|database)\s+url="\{*([^"}]+)\}*"[^>]*>\s*(.*?)\s*</\1>',
                  lambda m: f"{'📄' if m.group(1) == 'page' else '▦'} [{m.group(3).strip() or '无标题'}]({href(m.group(2))})", text, flags=re.S)

    def callout(m: re.Match) -> str:
        attrs, inner = m.group(1) or "", m.group(2).strip("\n")
        icon = (re.search(r'icon="([^"]*)"', attrs) or [None, ""])[1]
        color = (re.search(r'color="([^"]*)"', attrs) or [None, ""])[1]
        lines = [re.sub(r"^\t", "", x) for x in inner.split("\n")]
        return "\n".join([f"> [!{_callout_kind(icon, color)}]"] + [("> " + x) if x.strip() else ">" for x in lines])
    text = re.sub(r"<callout([^>]*)>\n?(.*?)\n?</callout>", callout, text, flags=re.S)

    def details(m: re.Match) -> str:
        inner = "\n".join(re.sub(r"^\t", "", x) for x in m.group(2).strip("\n").split("\n"))
        return f"<details><summary>{m.group(1).strip()}</summary>\n\n{inner}\n\n</details>"
    text = re.sub(r"<details[^>]*>\s*<summary>(.*?)</summary>\n?(.*?)\n?</details>", details, text, flags=re.S)
    text = re.sub(r"<table[^>]*>.*?</table>", lambda m: _table(m.group(0)), text, flags=re.S)
    text = re.sub(r'<image\s+source="([^"]+)"[^>]*>(.*?)</image>', lambda m: f"![{m.group(2).strip()}]({m.group(1)})", text, flags=re.S)
    text = re.sub(r'<image\s+source="([^"]+)"[^>]*/>', r"![](\1)", text)
    text = re.sub(r'<(file|pdf|video|audio)\s+source="([^"]+)"[^>]*>(.*?)</\1>',
                  lambda m: f"[{m.group(3).strip() or m.group(2).rsplit('/', 1)[-1]}]({m.group(2)})", text, flags=re.S)
    text = re.sub(r'<(file|pdf|video|audio)\s+source="([^"]+)"[^>]*/>', lambda m: f"[{m.group(2).rsplit('/', 1)[-1]}]({m.group(2)})", text)
    text = re.sub(r'<unknown\s+url="([^"]+)"(?:\s+alt="([^"]*)")?[^>]*/>', lambda m: f"[{m.group(2) or m.group(1)}]({m.group(1)})", text)
    text = re.sub(r"<li>(.*?)</li>", lambda m: f"- {m.group(1).strip()}" if m.group(1).strip() else "", text, flags=re.S)
    text = re.sub(r"</?(?:ul|ol|columns|column|synced_block|synced_block_reference|meeting-notes[a-z-]*)(?:\s[^>]*)?>", "", text)
    text = re.sub(r"<(?:empty-block|p|table_of_contents|breadcrumb)\s*/>", "", text)
    text = re.sub(r"[ \t]*\{(?:color|toggle)=\"[^\"]*\"\}[ \t]*$", "", text, flags=re.M)
    # 行内
    text = re.sub(r'<mention(?:-page|-database)?\s+url="\{*([^"}]+)\}*"[^>]*>(.*?)</mention(?:-page|-database)?>',
                  lambda m: f"[{m.group(2).strip() or '无标题'}]({href(m.group(1))})", text, flags=re.S)
    text = re.sub(r'<mention(?:-page|-database)?\s+url="\{*([^"}]+)\}*"[^>]*/>', lambda m: f"[页面]({href(m.group(1))})", text)
    text = re.sub(r"<mention-user[^>]*>(.*?)</mention-user>", lambda m: f"@{m.group(1).strip()}", text, flags=re.S)
    text = re.sub(r"<mention-user[^>]*/>", "", text)

    def date(m: re.Match) -> str:
        a = m.group(0)
        get = lambda k: (re.search(rf'{k}="([^"]*)"', a) or [None, ""])[1]   # noqa: E731
        s = get("start") + (" " + get("start-time") if get("start-time") else "")
        e = get("end") + (" " + get("end-time") if get("end-time") else "")
        return s + (f" → {e}" if e.strip() else "")
    text = re.sub(r"<(?:mention-)?date\s[^>]*/>", date, text)
    text = re.sub(r'<a\s+href="([^"]+)"[^>]*>(.*?)</a>', lambda m: f"[{m.group(2)}]({href(m.group(1))})", text, flags=re.S)
    for tags, mark in ((("b", "strong"), "**"), (("i", "em"), "*"), (("s", "del", "strike"), "~~")):
        for tg in tags:
            text = re.sub(rf"<{tg}>(.*?)</{tg}>", _wrap(mark), text, flags=re.S)
    text = re.sub(r"</?(?:u|span)(?:\s[^>]*)?>", "", text)
    text = re.sub(r"<br\s*/?>", "<br>", text)
    # 链接：Notion 的网址换成资料库里的
    text = re.sub(r"\]\((https?://[^)\s]*notion\.(?:so|site|com)[^)\s]*)\)", lambda m: f"]({href(m.group(1))})", text)
    text = re.sub(r"(?<![(<\[])(https?://(?:www\.|app\.)?notion\.(?:so|com)/[^\s)<>\]]+)",
                  lambda m: f"[页面]({l})" if (l := link(m.group(1))) else m.group(1), text)
    text = re.sub(r"\x00(\d+)\x00", lambda m: keep[int(m.group(1))], text)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip("\n") + "\n" if text.strip() else ""


# ---------- 数据库的属性 ----------
_FORMATS = {"number": "number", "number_with_commas": "comma", "percent": "percent", "yuan": "cny", "dollar": "usd"}
_GROUPS = {"to_do": "todo", "in_progress": "doing", "complete": "done"}
_SIMPLE = {"title": "title", "text": "text", "rich_text": "text", "number": "number", "select": "select", "multi_select": "multi_select",
           "status": "status", "date": "date", "checkbox": "checkbox", "url": "url", "email": "email", "phone_number": "phone",
           "file": "files", "files": "files", "auto_increment_id": "unique_id", "created_time": "date", "last_edited_time": "date",
           "relation": "relation"}


def _color(c) -> str:
    c = str(c or "default").replace("_background", "").replace("_bg", "")
    return c if c in dbs.COLORS else "default"


def map_schema(nschema: dict, order: list[str] | None = None) -> tuple[list[dict], dict]:
    """Notion 的表结构 → 资料库的（存的样子）。返回 (schema, {Notion 属性名: (我们的属性, Notion 的类型)})。
    关联先当文字建（导完所有库以后能对上的再换成关联，见 Import._relations）。"""
    names = list(nschema)
    if order:
        names = [n for n in order if n in nschema] + [n for n in names if n not in order]
    title = next((n for n in names if nschema[n].get("type") == "title"), None)
    schema = [{"key": "title", "name": (title or "名称")[:dbs.NAME_LEN], "type": "title"}]
    keys: dict = {}
    if title:
        keys[title] = (schema[0], "title")
    taken = {"title"}
    for n in names:
        p = nschema[n]
        nt = p.get("type") or "text"
        if nt == "title" or nt in ("button", "verification", "location", "place"):
            continue
        t = _SIMPLE.get(nt, "text")
        if t == "relation":
            t = "text"
        key = dbs._new_id("p", taken)
        taken.add(key)
        prop = {"key": key, "name": n[:dbs.NAME_LEN], "type": t}
        if t in dbs.OPTIONED:
            opts = []
            if t == "status":
                for g, items in (p.get("groups") or {}).items():
                    for o in items or []:
                        opts.append({"id": dbs._new_id("o", {x["id"] for x in opts}), "name": o["name"][:dbs.NAME_LEN],
                                     "color": _color(o.get("color")), "group": _GROUPS.get(g, "todo")})
            for o in p.get("options") or []:
                if not any(x["name"] == o["name"] for x in opts):
                    opts.append({"id": dbs._new_id("o", {x["id"] for x in opts}), "name": o["name"][:dbs.NAME_LEN], "color": _color(o.get("color"))})
            prop["options"] = opts
        if t == "number":
            prop["format"] = _FORMATS.get(p.get("format") or (p.get("number") or {}).get("format"), "number")
        if t == "unique_id":
            prop["prefix"], prop["next"] = "", 1
        schema.append(prop)
        keys[n] = (prop, nt)
    return schema, keys


def _json_list(v) -> list:
    if isinstance(v, list):
        return v
    if isinstance(v, str) and v.startswith("["):
        try:
            x = json.loads(v)
            return x if isinstance(x, list) else []
        except ValueError:
            return []
    return [v] if v not in (None, "") else []


def _dt(s: str | None, is_dt) -> str | None:
    if not s:
        return None
    s = str(s)
    return s[:10] + (" " + s[11:16] if is_dt and len(s) >= 16 else "")


def row_value(name: str, nt: str, row: dict, users: dict):
    """SQL 查出来的一行里某个属性 → 写进资料库的值（选项写名字，dbs.normalize 认）。"""
    if nt == "date":
        start = _dt(row.get(f"date:{name}:start"), row.get(f"date:{name}:is_datetime"))
        if not start:
            return None
        end = _dt(row.get(f"date:{name}:end"), row.get(f"date:{name}:is_datetime"))
        return {"start": start, **({"end": end} if end else {})}
    v = row.get(name)
    if nt == "checkbox":
        return v in ("__YES__", True, 1, "true") or None
    if nt == "multi_select":
        return _json_list(v) or None
    if nt in ("person", "people", "created_by", "last_edited_by"):
        return ", ".join(users.get(nid(x) or x, str(x)) for x in _json_list(v)) or None
    if nt in ("created_time", "last_edited_time"):
        return _dt(v, True) and {"start": _dt(v, True)}
    if nt == "number":
        if isinstance(v, str):
            try:
                v = float(v.replace(",", ""))
                v = int(v) if v.is_integer() else v
            except ValueError:
                return None
        return v if isinstance(v, (int, float)) and not isinstance(v, bool) else None
    if isinstance(v, (list, dict)):
        v = json.dumps(v, ensure_ascii=False)
    if isinstance(v, str):
        return v[:dbs.MAX_TEXT] or None
    return v


def _int(v) -> int | None:
    if isinstance(v, int) and not isinstance(v, bool):
        return v
    m = re.search(r"(\d+)\s*$", str(v or ""))
    return int(m.group(1)) if m else None


def _inline(v: str, link) -> str:
    """属性里的文字也带 Notion 的标记（<mention>、加粗）：转成 Markdown，链接换成资料库里的，合成一行。"""
    if "<" not in v and "notion." not in v:
        return v
    return " ".join(to_markdown(v, link).split("\n")).strip()


# ---------- 导入 ----------
JOBS: dict[int, dict] = {}
_seq = 0


def mapped(n: str) -> int | None:
    r = conn.execute("select page_id from notion_map where nid=?", (n,)).fetchone()
    if r and library.get_page(r[0], False):
        return r[0]
    return None


def _remember(n: str, pid: int, kind: str):
    conn.execute("insert or replace into notion_map(nid, page_id, kind, synced) values(?,?,?,?)", (n, pid, kind, time.time()))
    conn.commit()


def imported() -> list[dict]:
    """导入过的根（没有上级也是导入的）：[{nid, page_id, title, synced}]。"""
    out = []
    for n, pid, kind, synced in conn.execute("select nid, page_id, kind, synced from notion_map where kind in ('page', 'database') order by synced desc"):
        p = library.get_page(pid, False)
        if p and p["parent_id"] is None:
            out.append({"nid": n, "page_id": pid, "title": p["title"], "kind": p["kind"], "synced": synced})
    return out


class Import:
    """一次导入：爬 → 建页面和库 → 建行 → 写正文（链接都对上以后）→ 能对上的关联换成真的关联。"""

    def __init__(self, roots: list[str], parent_id: int | None = None):
        self.roots = [r for r in (nid(x) for x in roots) if r]
        self.parent_id = parent_id
        self.nodes: dict[str, dict] = {}       # Notion 编号 → 爬到的页面 / 库（parsed + parent + 资料库编号）
        self.order: list[str] = []
        self.sources: dict[str, str] = {}      # 数据源 collection:// → 第一个用它的库的 Notion 编号
        self.alias: dict[str, str] = {}        # “View of …”库 → 真的那个库
        self.rows: dict[str, dict] = {}        # 行的 Notion 编号 → {db, row（SQL 的一行）, page_id}
        self.users: dict[str, str] = {}
        self.log: list[str] = []
        self.counts = {"pages": 0, "dbs": 0, "rows": 0, "updated": 0, "files": 0}
        self.state = "running"
        self.step = ""
        self.error = ""

    def public(self) -> dict:
        return {"state": self.state, "step": self.step, "counts": self.counts, "log": self.log[-30:], "error": self.error,
                "roots": [{"nid": r, "page_id": self.nodes.get(r, {}).get("page_id")} for r in self.roots]}

    def note(self, s: str):
        self.log.append(s)

    def link(self, url: str) -> str | None:
        n = nid(url)
        if not n:
            return None
        n = self.alias.get(n, n)
        pid = (self.nodes.get(n) or {}).get("page_id") or (self.rows.get(n) or {}).get("page_id") or mapped(n)
        return library.link(pid) if pid else None

    async def run(self):
        try:
            await self.crawl()
            await self.load_rows()
            self.create()
            await self.fill()
            self.state = "done"
            self.step = ""
        except Exception as e:   # 导到一半出错：已经导进来的留着，再导一次会接着更新
            self.state = "failed"
            self.error = str(e)[:500] or type(e).__name__

    async def crawl(self):
        self.step = "crawl"
        todo = [(r, None) for r in self.roots]
        while todo:
            n, parent = todo.pop(0)
            if n in self.nodes or n in self.alias:
                continue
            if len(self.nodes) >= MAX_NODES:
                self.note(f"页面和库超过 {MAX_NODES} 个，后面的没导")
                break
            d = parse_fetch(await call("notion-fetch", {"id": n}))
            d["id"], d["parent"] = n, parent
            self.nodes[n] = d
            self.order.append(n)
            if d["type"] == "page":
                kids = [k for _, k, _ in children(d["content"])]
                todo[0:0] = [(k, n) for k in kids]   # 先序：子页面紧跟着上级，顺序和正文里一样
        # 几个库用同一份数据（Notion 的“View of …”是链到原库的视图，常带着筛选）：只建原库，别的当成指向它的链接
        by_src: dict[str, list[str]] = {}
        for n in self.order:
            d = self.nodes[n]
            if d["type"] == "database" and d["sources"]:
                by_src.setdefault(d["sources"][0]["url"], []).append(n)
        for src, ns in by_src.items():
            real = min(ns, key=lambda k: (self.nodes[k]["title"].startswith("View of "), self.order.index(k)))
            self.sources[src] = real
            for k in ns:
                if k != real:
                    self.alias[k] = real
                    del self.nodes[k]
        self.order = [n for n in self.order if n in self.nodes]

    async def load_rows(self):
        self.step = "rows"
        try:
            users = json.loads(await call("notion-get-users", {}))
            for u in users.get("results") or users.get("users") or []:
                if isinstance(u, dict) and u.get("id"):
                    self.users[nid(u["id"]) or u["id"]] = u.get("name") or ""
        except Exception:
            pass
        for n in self.order:
            d = self.nodes[n]
            if d["type"] != "database" or not d["sources"]:
                continue
            rows = await self._query(d)
            auto = next((k for k, p in d["sources"][0]["schema"].items() if p.get("type") == "auto_increment_id"), None)
            rows.sort(key=lambda r: _int(r.get(auto)) or 0 if auto else (r.get("createdTime") or ""))
            d["rows"] = rows
            for r in rows:
                if rid := nid(r.get("url") or r.get("id")):
                    self.rows[rid] = {"db": n, "row": r, "page_id": mapped(rid)}

    async def _query(self, d: dict) -> list[dict]:
        """一个库的所有行：先按它的视图查（Notion 免费版的 SQL 查询每小时有限额，按视图查不算），没有视图再用 SQL。"""
        views = [v for v in d["views"] if str(v.get("url", "")).startswith("view://")]
        plain = [v for v in views if not v.get("filter") and not v.get("filters")]   # 带筛选的视图查不全
        view = (plain or views or [{}])[0].get("url")
        if view:
            got = json.loads(await call("notion-query-data-sources", {"data": {"mode": "view", "view_url": view}}))
            if got.get("has_more"):
                self.note(f"「{d['title']}」行太多，只导了前 {len(got.get('results') or [])} 行")
            return got.get("results") or []
        src = d["sources"][0]["url"]
        rows, offset = [], 0
        while True:
            q = f'SELECT * FROM "{src}" LIMIT {PAGE_ROWS} OFFSET {offset}'
            got = json.loads(await call("notion-query-data-sources", {"data": {"mode": "sql", "data_source_urls": [src], "query": q}}))
            batch = got.get("results") or []
            rows += batch
            if len(batch) < PAGE_ROWS:
                return rows
            offset += PAGE_ROWS

    def create(self):
        """建页面和库（正文先空着，等所有编号都有了再写）。导过的就沿用。"""
        self.step = "create"
        for n in self.order:
            d = self.nodes[n]
            parent = self.nodes[d["parent"]]["page_id"] if d["parent"] else self.parent_id
            old = mapped(n)
            if d["type"] == "database":
                if old:
                    d["page_id"] = old
                    continue
                order = next((v.get("displayProperties") for v in d["views"] if v.get("displayProperties")), None)
                schema, _ = map_schema(d["sources"][0]["schema"] if d["sources"] else {}, order)
                db = dbs.create(d["title"] or "无标题数据库", "user", schema=schema, parent_id=parent)
                d["page_id"] = db["id"]
                _remember(n, db["id"], "database")
                self.counts["dbs"] += 1
            else:
                if old:
                    d["page_id"] = old
                    continue
                p = library.save_page(None, d["title"] or "无标题", "", "markdown", by="user", allow_empty=True, parent_id=parent)
                d["page_id"] = p["id"]
                _remember(n, p["id"], "page")
                self.counts["pages"] += 1

    async def fill(self):
        self.step = "content"
        dbs_ = [self.nodes[n] for n in self.order if self.nodes[n]["type"] == "database"]
        for d in dbs_:   # 先把所有行建出来，正文、属性里提到别的行时链接才对得上
            self._make_rows(d)
        for d in dbs_:
            await self.fill_db(d)
        for n in self.order:
            d = self.nodes[n]
            if d["type"] == "page":
                self._write(d["page_id"], d["title"], to_markdown(d["content"], self.link))
        self.step = "relations"
        self._relations()

    def _write(self, pid: int, title: str, md: str, kind: str = "page"):
        p = library.get_page(pid)
        title = title if title or kind == "row" else p["title"]
        if p and (p["content"] or "") == md and p["title"] == title:
            return
        library.save_page(None, title, md, "markdown", pid=pid, by="user", allow_empty=True, kind=kind)
        if p and p["content"]:
            self.counts["updated"] += 1

    def _title_key(self, d: dict) -> str | None:
        return next((k for k, p in (d["sources"][0]["schema"] if d["sources"] else {}).items() if p.get("type") == "title"), None)

    def _make_rows(self, d: dict):
        tk = self._title_key(d)
        for r in d.get("rows") or []:
            rid = nid(r.get("url") or r.get("id"))
            entry = self.rows.get(rid) if rid else None
            if not entry or (entry.get("page_id") and dbs.get_row(entry["page_id"])):
                continue
            row = dbs.add_row(d["page_id"], {}, "user", title=str(r.get(tk) or "")[:120] if tk else "")
            entry["page_id"] = row["id"]
            entry["new"] = True
            _remember(rid, row["id"], "row")
            self.counts["rows"] += 1

    async def fill_db(self, d: dict):
        """每一行：属性按 Notion 现在的写（新建的、导过的都一样），正文从行的页面取。"""
        db = dbs.get_db(d["page_id"])
        _, keys = map_schema(d["sources"][0]["schema"] if d["sources"] else {})
        by_name = {p["name"]: p for p in db["schema"]}
        uid = next((p for p in db["schema"] if p["type"] == "unique_id"), None)
        prefix = ""
        for r in d.get("rows") or []:
            rid = nid(r.get("url") or r.get("id"))
            entry = self.rows.get(rid) if rid else None
            if not entry or not entry.get("page_id"):
                continue
            page = parse_fetch(await call("notion-fetch", {"id": rid}))
            props, title, number = {}, "", None
            for name, (_, nt) in keys.items():
                prop = by_name.get(name[:dbs.NAME_LEN])
                if not prop:
                    continue
                if nt == "title":
                    title = _inline(str(r.get(name) or page["props"].get(name) or ""), self.link)
                    continue
                if prop["type"] == "unique_id":
                    number = _int(r.get(name))
                    if m := re.match(r"^(.*?)(\d+)$", str(page["props"].get(name) or "")):
                        prefix = m.group(1)
                    continue
                v = row_value(name, nt, r, self.users)
                if prop["type"] in ("text", "url") and isinstance(v, str):
                    v = _inline(v, self.link)[:dbs.MAX_TEXT] or None
                if prop["type"] == "files":   # 行页面的属性里是临时的下载地址（几分钟就失效），马上下载
                    v = await self._files([u for u in _json_list(page["props"].get(name)) if str(u).startswith("http")])
                props[prop["key"]] = v
            row_id = entry["page_id"]
            try:
                changed = dbs.update_row(row_id, {**props, "title": title}, "user")[1]
            except ValueError:   # 有认不出的值：一个一个写，认不出的跳过
                changed = []
                for k, v in {**props, "title": title}.items():
                    try:
                        changed += dbs.update_row(row_id, {k: v}, "user")[1]
                    except ValueError as e:
                        self.note(f"「{title}」的一个属性没导：{e}")
            if changed and not entry.get("new"):
                self.counts["updated"] += 1
            fields, args = [], []
            if entry.get("new") and (created := r.get("createdTime")):
                try:
                    fields.append("created=?")
                    args.append(time.mktime(time.strptime(str(created)[:19].replace("T", " "), "%Y-%m-%d %H:%M:%S")) - time.timezone)
                except ValueError:
                    fields.pop()
            if uid and isinstance(number, int):
                cur = dbs.get_row(row_id)["props"]
                if cur.get(uid["key"]) != number:
                    cur[uid["key"]] = number
                    fields.append("props=?")
                    args.append(json.dumps(cur, ensure_ascii=False))
            if fields:
                conn.execute(f"update pages set {', '.join(fields)} where id=?", (*args, row_id))
                conn.commit()
            self._write(row_id, title, to_markdown(page["content"], self.link), "row")
        # 自动编号：前缀用 Notion 显示的（TASK-），下一个号接着最大的
        db = dbs.get_db(d["page_id"])
        for p in db["schema"]:
            if p["type"] == "unique_id":
                nums = [x["props"].get(p["key"]) for x in dbs.rows(db["id"], db) if isinstance(x["props"].get(p["key"]), int)]
                p["next"] = max(nums, default=0) + 1
                if prefix and not p.get("prefix"):
                    p["prefix"] = prefix
                dbs._touch(db["id"], schema=db["schema"])

    async def _files(self, urls: list) -> list | None:
        """Notion 里存的文件：下载下来存进资料库（任何文件都收）。下载不了的跳过，说一声。"""
        if not urls:
            return None
        import httpx
        out = []
        async with httpx.AsyncClient(timeout=60, follow_redirects=True) as h:
            for url in urls:
                name = str(url).split("?")[0].rstrip("/").rsplit("/", 1)[-1] or "file"
                try:
                    r = await h.get(url)
                    r.raise_for_status()
                    m = media.save(r.content, name, limit=media.MAX_BYTES, any_file=True)
                    out.append({"name": m["name"], "url": media.url(m["id"])})
                    self.counts["files"] += 1
                except Exception as e:
                    self.note(f"文件 {name} 没下载：{str(e)[:80]}")
        return out or None

    def _relations(self):
        """Notion 的关联：对方的库也导进来了就换成真的关联（值是对方的行），不然留着文字。"""
        for n in self.order:
            d = self.nodes[n]
            if d["type"] != "database" or not d["sources"]:
                continue
            for name, p in d["sources"][0]["schema"].items():
                if p.get("type") != "relation":
                    continue
                target_src = (p.get("relation") or {}).get("data_source_id") or p.get("dataSourceUrl") or p.get("collection_url") or ""
                target = next((self.nodes[k]["page_id"] for s, k in self.sources.items() if nid(s) and nid(s) == nid(target_src)), None)
                if not target:
                    continue
                db = dbs.get_db(d["page_id"])
                prop = next((x for x in db["schema"] if x["name"] == name[:dbs.NAME_LEN]), None)
                if not prop or prop["type"] == "relation":
                    continue
                values = {}
                for rid, e in self.rows.items():
                    if e["db"] == n and e.get("page_id"):
                        ids = [self.rows[t]["page_id"] for t in (nid(x) for x in _json_list(e["row"].get(name))) if t in self.rows and self.rows[t].get("page_id")]
                        values[e["page_id"]] = ids
                dbs.change_schema(db["id"], [{"op": "update", "prop": prop["key"], "type": "relation", "db": target}], "user")
                for row_id, ids in values.items():
                    if ids:
                        dbs.update_row(row_id, {prop["key"]: ids}, "user")


def start(roots: list[str], parent_id: int | None = None) -> int:
    global _seq
    if not roots:
        raise ValueError(i18n.t("选几个要导入的 Notion 页面或数据库"))
    if mcp_hub.session_of(APP) is None:
        raise NotConnected(i18n.t("Notion 没连上：先到应用市场连上 Notion"))
    if any(j.state == "running" for j in JOBS.values()):
        raise ValueError(i18n.t("已经在导入了，等这次导完"))
    _seq += 1
    job = Import(roots, parent_id)
    JOBS[_seq] = job
    asyncio.create_task(job.run())
    return _seq
