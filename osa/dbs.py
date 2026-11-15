"""资料库的数据库（多维表，参考 Notion）：一个库是一个页面（pages.kind = db），每一行是它下面的子页面（kind = row）。

- 表结构 pages.schema：[{key, name, type, options?, format?}]，第一项是名称（key = title，就是行页面的标题，不能删）。
  选项 {id, name, color, group?}（状态的选项多一个 group：todo / doing / done）。值里存选项 id，改名不用动每一行。
- 视图 pages.views：[{id, name, type: table | board, sort, filter, group_by?, hidden, widths}]。
- 行的属性 pages.props：{key: 值}，空的不存。props_meta：{key: {v, by, ts}}，每个键最后在哪个版本、被谁改的：
  给了版本号时只有要改的键在那之后被别人改过才冲突（和段的 block_meta 一个思路）。名称的改动也记在 props_meta.title 里。
- 行的正文还是 Markdown，按段编辑、评论、记历史都照旧（pagedoc.py）；属性改动也记一版历史（page_versions.props）。
- 值有两种写法：网页发的是存的样子（选项 id、bot:3、{start, end}）；工具（Bot、外部 Agent）写名字（“进行中”、“Nova”、“2026-11-15”）。
  normalize 两种都认。工具读到的一律是名字。
- 改表结构不记历史、不算冲突，按属性后写的为准；改类型时能转的值转过去（先变成文字再按新类型认），转不了的清空。
- 筛选、排序在这里做，工具和网页用同一套。
"""
import json
import re
import secrets
import time
from datetime import date, timedelta
from . import bots, i18n, library, pagedoc
from .memory import conn

TYPES = {"title": "名称", "text": "文本", "number": "数字", "select": "单选", "multi_select": "多选", "status": "状态",
         "date": "日期", "person": "人员", "checkbox": "勾选", "url": "网址", "email": "邮箱", "phone": "电话",
         "created_time": "创建时间", "updated_time": "最后编辑时间", "created_by": "创建人", "updated_by": "最后编辑人"}
COMPUTED = {"created_time", "updated_time", "created_by", "updated_by"}
OPTIONED = {"select", "multi_select", "status"}
TEXTUAL = {"title", "text", "url", "email", "phone"}
COLORS = ("default", "gray", "brown", "orange", "yellow", "green", "blue", "purple", "pink", "red")
GROUPS = ("todo", "doing", "done")
GROUP_NAMES = {"todo": "未开始", "doing": "进行中", "done": "已完成"}
FORMATS = ("number", "comma", "percent", "cny", "usd")
OPS = ("eq", "neq", "contains", "not_contains", "gt", "gte", "lt", "lte", "empty", "not_empty")
MAX_ROWS = 1000
MAX_PROPS = 50
MAX_OPTIONS = 100
MAX_VIEWS = 20
MAX_TEXT = 2000
NAME_LEN = 60
_BY_ZH = {v: k for k, v in TYPES.items()}


class Conflict(Exception):
    """要改的属性在调用方读过之后被别人改了。items：[{prop, name, current, by}]（current 是给人看的值）。"""

    def __init__(self, version: int, items: list[dict]):
        super().__init__("conflict")
        self.version = version
        self.items = items


# ---------- 读库 ----------
def _new_id(prefix: str, taken) -> str:
    while True:
        k = prefix + secrets.token_hex(2)
        if k not in taken:
            return k


def get_db(db_id) -> dict | None:
    """库页面加上 schema、views。不是库返回 None。"""
    p = library.get_page(db_id, False) if isinstance(db_id, int) and not isinstance(db_id, bool) else None
    if not p or p["kind"] != "db":
        return None
    r = conn.execute("select schema, views from pages where id=?", (db_id,)).fetchone()
    return {**p, "schema": json.loads(r[0] or "[]"), "views": json.loads(r[1] or "[]")}


def _row_of(r, last_by: dict) -> dict:
    i, title, version, pos, created, updated, bot_id, agent_id, props, meta, locked = r
    made = f"bot:{bot_id}" if bot_id else f"agent:{agent_id}" if agent_id else "user"
    return {"id": i, "title": title, "version": version or 0, "pos": pos or 0, "created": created, "updated": updated,
            "created_by": made, "updated_by": last_by.get(i, made), "props": json.loads(props or "{}"),
            "meta": json.loads(meta or "{}"), "locked": bool(locked)}


_ROW_COLS = "id, title, version, pos, created, updated, bot_id, agent_id, props, props_meta, locked"


def _last_by(ids: list[int]) -> dict:
    """每一行最后是谁改的（一次查完，不一行一查）。"""
    out = {}
    for k in range(0, len(ids), 500):
        part = ids[k:k + 500]
        q = (f"select page_id, by from page_versions where id in (select max(id) from page_versions where label != 'base' "
             f"and page_id in ({','.join('?' * len(part))}) group by page_id)")
        out |= {pid: by for pid, by in conn.execute(q, part)}
    return out


def rows(db_id: int) -> list[dict]:
    """库里的所有行，按手动排的顺序。"""
    rs = conn.execute(f"select {_ROW_COLS} from pages where parent_id=? and kind='row' order by coalesce(pos, 0), id", (db_id,)).fetchall()
    last = _last_by([r[0] for r in rs])
    return [_row_of(r, last) for r in rs]


def get_row(row_id) -> dict | None:
    if not isinstance(row_id, int) or isinstance(row_id, bool):
        return None
    r = conn.execute(f"select {_ROW_COLS}, parent_id from pages where id=? and kind='row'", (row_id,)).fetchone()
    if not r:
        return None
    return {**_row_of(r[:-1], _last_by([r[0]])), "db_id": r[-1]}


def prop_of(db: dict, ref) -> dict | None:
    """按 key 或名字找属性（名字不分大小写）。"""
    ref = str(ref or "").strip()
    for p in db["schema"]:
        if p["key"] == ref:
            return p
    low = ref.lower()
    return next((p for p in db["schema"] if p["name"].lower() == low), None)


def value_of(row: dict, prop: dict):
    """行在这一列的值（存的样子）。名称、自动算的从页面取。"""
    t = prop["type"]
    if t == "title":
        return row["title"]
    if t in ("created_time", "updated_time"):
        return row["created" if t == "created_time" else "updated"]
    if t in ("created_by", "updated_by"):
        return row[t]
    return row["props"].get(prop["key"])


# ---------- 值：认、显示 ----------
def person_name(by: str) -> str:
    return pagedoc.name_of(by).removesuffix("（外部 Agent）") if by.startswith("agent:") else pagedoc.name_of(by)


def _people() -> dict[str, str]:
    """名字（小写）→ 作者：用户、Bot、外部 Agent。"""
    from . import agentapi
    out = {"user": "user", "用户": "user"}
    for b in bots.list_bots():
        out[b["name"].lower()] = f"bot:{b['id']}"
    for a in agentapi.names():
        out.setdefault(a["name"].lower(), f"agent:{a['id']}")
    return out


def _day(s: str) -> str | None:
    """日期：2026-11-15、2026/11/15、2026-11-15 14:00、2026-11-15T14:00，today / 今天 / 明天 / 昨天。"""
    s = str(s or "").strip()
    rel = {"today": 0, "今天": 0, "tomorrow": 1, "明天": 1, "yesterday": -1, "昨天": -1}
    if s.lower() in rel:
        return (date.today() + timedelta(days=rel[s.lower()])).isoformat()
    m = re.fullmatch(r"(\d{4})[-/.](\d{1,2})[-/.](\d{1,2})(?:[ T](\d{1,2}):(\d{2})(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?)?", s)
    if not m:
        return None
    y, mo, d, hh, mm = m.groups()
    try:
        day = date(int(y), int(mo), int(d)).isoformat()
    except ValueError:
        return None
    if hh is not None:
        if int(hh) > 23 or int(mm) > 59:
            return None
        return f"{day} {int(hh):02d}:{mm}"
    return day


def _truthy(v) -> bool | None:
    if isinstance(v, bool):
        return v
    s = str(v).strip().lower()
    if s in ("1", "true", "yes", "y", "是", "✓", "✔", "x", "[x]", "done", "对", "checked"):
        return True
    if s in ("0", "false", "no", "n", "否", "", "[ ]", "不是", "unchecked"):
        return False
    return None


def _option(prop: dict, v, create: bool) -> str:
    """选项：给的是 id 或名字。create：单选、多选里没有的名字自动加一个（状态不加）。"""
    opts = prop.setdefault("options", [])
    s = str(v).strip()
    for o in opts:
        if o["id"] == s:
            return o["id"]
    for o in opts:
        if o["name"].lower() == s.lower():
            return o["id"]
    if not create or prop["type"] == "status":
        names = "、".join(o["name"] for o in opts) or "（还没有选项）"
        raise ValueError(f"「{prop['name']}」没有选项「{s}」，可选：{names}")
    if not s or len(s) > NAME_LEN:
        raise ValueError(f"「{prop['name']}」的选项名不能是空的、也不能超过 {NAME_LEN} 字")
    if len(opts) >= MAX_OPTIONS:
        raise ValueError(f"「{prop['name']}」最多 {MAX_OPTIONS} 个选项")
    o = {"id": _new_id("o", {x["id"] for x in opts}), "name": s, "color": COLORS[1 + len(opts) % (len(COLORS) - 1)]}
    opts.append(o)
    return o["id"]


def normalize(prop: dict, v, create: bool = True):
    """把给的值变成存的样子；None / 空串 / 空列表是清空（返回 None）。认不出抛 ValueError（说清楚该怎么写）。
    create 时单选、多选会自动加没有的选项（改的是 prop 本身，调用方负责把表结构存回去）。"""
    t, name = prop["type"], prop["name"]
    if v is None or v == "" or v == []:
        return None
    if t in COMPUTED:
        raise ValueError(f"「{name}」是自动算的，不能改")
    if t in ("title", "text", "url", "email", "phone"):
        if isinstance(v, (dict, list)):
            raise ValueError(f"「{name}」要写一段文字")
        s = str(v).strip()
        if t != "text":
            s = " ".join(s.split())
        if len(s) > MAX_TEXT:
            raise ValueError(f"「{name}」太长（最多 {MAX_TEXT} 字）")
        if t == "email" and s and not re.fullmatch(r"[^@\s]+@[^@\s]+\.[^@\s]+", s):
            raise ValueError(f"「{name}」不像邮箱：{s}")
        if t == "url" and s and not re.match(r"[a-z][a-z0-9+.-]*://|mailto:|/", s, re.I):
            s = "https://" + s
        return s or None
    if t == "number":
        if isinstance(v, bool):
            raise ValueError(f"「{name}」要写数字")
        if isinstance(v, (int, float)):
            n = v
        else:
            s = re.sub(r"[,，\s¥$￥%]", "", str(v))
            try:
                n = float(s)
            except ValueError:
                raise ValueError(f"「{name}」要写数字，不是「{v}」") from None
        if n != n or n in (float("inf"), float("-inf")):
            raise ValueError(f"「{name}」要写数字")
        return int(n) if isinstance(n, float) and n.is_integer() and abs(n) < 2 ** 53 else n
    if t in ("select", "status"):
        if isinstance(v, list):
            if len(v) != 1:
                raise ValueError(f"「{name}」只能选一个")
            v = v[0]
        return _option(prop, v, create)
    if t == "multi_select":
        items = v if isinstance(v, list) else [x for x in re.split(r"[,，、;；]", str(v))]
        out = []
        for x in items:
            if str(x).strip():
                oid = _option(prop, x, create)
                if oid not in out:
                    out.append(oid)
        return out or None
    if t == "date":
        if isinstance(v, dict):
            start, end = v.get("start"), v.get("end")
        else:
            parts = re.split(r"\s*(?:→|~|～|到|至| - | to )\s*", str(v).strip(), maxsplit=1)
            start, end = parts[0], parts[1] if len(parts) > 1 else None
        a = _day(start)
        if not a:
            raise ValueError(f"「{name}」的日期写成 2026-11-15 或 2026-11-15 14:00，不是「{start}」")
        out = {"start": a}
        if end not in (None, ""):
            b = _day(end)
            if not b:
                raise ValueError(f"「{name}」的结束日期写成 2026-11-15 或 2026-11-15 14:00，不是「{end}」")
            if b < a:
                raise ValueError(f"「{name}」的结束日期在开始日期前面")
            out["end"] = b
        return out
    if t == "person":
        items = v if isinstance(v, list) else [x for x in re.split(r"[,，、;；]", str(v))]
        people, out = _people(), []
        for x in items:
            s = str(x).strip()
            if not s:
                continue
            if re.fullmatch(r"(bot|agent):\d+", s) or s == "user":
                who = s
            else:
                who = people.get(s.lower())
                if not who:
                    raise ValueError(f"「{name}」没有叫「{s}」的人：写“用户”或者 Bot、外部 Agent 的名字")
            if who not in out:
                out.append(who)
        return out or None
    if t == "checkbox":
        b = _truthy(v)
        if b is None:
            raise ValueError(f"「{name}」要写 true 或 false")
        return b or None   # 没勾就是空的
    raise ValueError(f"不认识的类型：{t}")


def _num(prop: dict, n) -> str:
    f = prop.get("format") or "number"
    if f == "comma":
        return f"{n:,}"
    if f == "percent":
        return f"{n}%"
    if f == "cny":
        return f"¥{n:,}"
    if f == "usd":
        return f"${n:,}"
    return str(n)


def display(prop: dict, v) -> str:
    """给人和模型看的值（选项写名字、人写名字）。空的是空串。"""
    if v is None or v == "" or v == []:
        return ""
    t = prop["type"]
    if t == "number":
        return _num(prop, v)
    if t in ("select", "status", "multi_select"):
        names = {o["id"]: o["name"] for o in prop.get("options") or []}
        ids = v if isinstance(v, list) else [v]
        return ", ".join(names.get(i, "?") for i in ids)
    if t == "date":
        return v["start"] + (f" → {v['end']}" if v.get("end") else "") if isinstance(v, dict) else str(v)
    if t in ("person", "created_by", "updated_by"):
        return ", ".join(person_name(x) for x in (v if isinstance(v, list) else [v]))
    if t == "checkbox":
        return "是" if v else ""
    if t in ("created_time", "updated_time"):
        return time.strftime("%Y-%m-%d %H:%M", time.localtime(v))
    return str(v)


# ---------- 表结构 ----------
def _clean_options(prop: dict, given, keep: list[dict]) -> list[dict]:
    """给的选项列表（名字，或者 {id?, name, color?, group?}）→ 存的样子。同名的沿用原来的 id 和颜色。"""
    if not isinstance(given, list):
        raise ValueError(f"「{prop['name']}」的 options 要是一个列表")
    if len(given) > MAX_OPTIONS:
        raise ValueError(f"「{prop['name']}」最多 {MAX_OPTIONS} 个选项")
    by_id = {o["id"]: o for o in keep}
    by_name = {o["name"].lower(): o for o in keep}
    out, seen = [], set()
    for k, g in enumerate(given):
        g = {"name": g} if isinstance(g, str) else g if isinstance(g, dict) else None
        if g is None:
            raise ValueError(f"「{prop['name']}」的选项要写名字")
        name = " ".join(str(g.get("name") or "").split())[:NAME_LEN]
        if not name or name.lower() in seen:
            raise ValueError(f"「{prop['name']}」的选项名不能是空的、也不能重复：{name or '（空）'}")
        seen.add(name.lower())
        old = by_id.get(g.get("id")) or by_name.get(name.lower())
        color = g.get("color") if g.get("color") in COLORS else old["color"] if old else COLORS[1 + k % (len(COLORS) - 1)]
        o = {"id": old["id"] if old else _new_id("o", {x["id"] for x in out} | set(by_id)), "name": name, "color": color}
        if prop["type"] == "status":
            o["group"] = g.get("group") if g.get("group") in GROUPS else old.get("group") if old and old.get("group") else (
                "done" if re.search(r"完成|done|关闭|closed", name, re.I) else "doing" if re.search(r"进行|doing|progress|处理中", name, re.I) else "todo")
        out.append(o)
    return out


def _status_default() -> list[dict]:
    return [{"id": "o" + secrets.token_hex(2), "name": GROUP_NAMES[g], "color": c, "group": g}
            for g, c in (("todo", "gray"), ("doing", "blue"), ("done", "green"))]


def _type(v) -> str:
    t = str(v or "").strip()
    t = _BY_ZH.get(t, t)
    if t not in TYPES or t == "title":
        raise ValueError(f"不认识的属性类型「{v}」，可以是：{'、'.join(k for k in TYPES if k != 'title')}")
    return t


def _new_prop(schema: list[dict], spec: dict) -> dict:
    name = " ".join(str(spec.get("name") or "").split())[:NAME_LEN]
    if not name:
        raise ValueError("属性要有名字")
    if any(p["name"].lower() == name.lower() for p in schema):
        raise ValueError(f"已经有叫「{name}」的属性了")
    t = _type(spec.get("type"))
    p = {"key": _new_id("p", {x["key"] for x in schema}), "name": name, "type": t}
    if t in OPTIONED:
        p["options"] = _clean_options(p, spec["options"], []) if spec.get("options") is not None else (_status_default() if t == "status" else [])
    if t == "number":
        p["format"] = spec.get("format") if spec.get("format") in FORMATS else "number"
    return p


def default_schema() -> list[dict]:
    return [{"key": "title", "name": "名称", "type": "title"},
            {"key": "status", "name": "状态", "type": "status", "options": _status_default()},
            {"key": "owner", "name": "负责人", "type": "person"},
            {"key": "due", "name": "日期", "type": "date"}]


def _default_views(schema: list[dict]) -> list[dict]:
    views = [{"id": "v1", "name": "表格", "type": "table"}]
    group = next((p["key"] for p in schema if p["type"] == "status"), None) or next((p["key"] for p in schema if p["type"] == "select"), None)
    if group:
        views.append({"id": "v2", "name": "看板", "type": "board", "group_by": group})
    return views


def create(title: str, by: str, schema: list[dict] | None = None, parent_id: int | None = None, bot_id: int | None = None,
           properties: list[dict] | None = None, title_name: str | None = None) -> dict:
    """建一个库。schema 是存的样子（网页用 default_schema）；properties 是工具给的 [{name, type, options?, format?}]。"""
    if schema is None:
        schema = [{"key": "title", "name": " ".join(str(title_name or "").split())[:NAME_LEN] or "名称", "type": "title"}]
        for spec in properties or []:
            if len(schema) > MAX_PROPS:
                raise ValueError(f"最多 {MAX_PROPS} 个属性")
            schema.append(_new_prop(schema, spec if isinstance(spec, dict) else {"name": spec}))
    p = library.save_page(bot_id, title, "", "markdown", by=by, allow_empty=True, parent_id=parent_id, kind="db")
    conn.execute("update pages set schema=?, views=? where id=?",
                 (json.dumps(schema, ensure_ascii=False), json.dumps(_default_views(schema), ensure_ascii=False), p["id"]))
    conn.commit()
    return get_db(p["id"])


def _touch(db_id: int, schema: list[dict] | None = None, views: list[dict] | None = None):
    sets, args = ["updated=?"], [time.time()]
    if schema is not None:
        sets.append("schema=?")
        args.append(json.dumps(schema, ensure_ascii=False))
    if views is not None:
        sets.append("views=?")
        args.append(json.dumps(views, ensure_ascii=False))
    conn.execute(f"update pages set {', '.join(sets)} where id=?", (*args, db_id))
    conn.commit()


def _rewrite_values(db_id: int, key: str, fn):
    """改表结构时改每一行这一列的值（fn(旧值) → 新值，None 是清空）。不记历史、不涨版本号：这是表结构的事。"""
    for r in conn.execute("select id, props from pages where parent_id=? and kind='row'", (db_id,)).fetchall():
        props = json.loads(r[1] or "{}")
        if key not in props:
            continue
        new = fn(props[key])
        if new == props[key]:
            continue
        if new is None:
            props.pop(key)
        else:
            props[key] = new
        conn.execute("update pages set props=? where id=?", (json.dumps(props, ensure_ascii=False), r[0]))


def change_schema(db_id: int, ops: list[dict]) -> dict:
    """改表结构：{op: add, name, type, options?, format?} / {op: update, prop, name?, type?, options?, format?} / {op: delete, prop}。
    prop 是 key 或名字。按顺序做，有一个不行整批不做。返回改好的库。"""
    db = get_db(db_id)
    if not db:
        raise LookupError(i18n.t("没有数据库 #{id}", id=db_id))
    if not isinstance(ops, list) or not ops:
        raise ValueError("没有要改的")
    schema = json.loads(json.dumps(db["schema"]))
    views = db["views"]
    later: list = []   # 表结构都通过了才改行里的值
    for op in ops:
        if not isinstance(op, dict):
            raise ValueError("每个改动都要是一个对象")
        kind = op.get("op")
        if kind == "add":
            if len(schema) > MAX_PROPS:
                raise ValueError(f"最多 {MAX_PROPS} 个属性")
            schema.append(_new_prop(schema, op))
            continue
        prop = prop_of({"schema": schema}, op.get("prop"))
        if not prop:
            raise ValueError(f"没有属性「{op.get('prop')}」，现在有：{'、'.join(p['name'] for p in schema)}")
        if kind == "delete":
            if prop["type"] == "title":
                raise ValueError("名称不能删")
            schema.remove(prop)
            later.append(("drop", prop["key"], None, None))
            continue
        if kind != "update":
            raise ValueError(f"不认识的改动：{kind}（只能是 add、update、delete）")
        if op.get("name") is not None:
            name = " ".join(str(op["name"]).split())[:NAME_LEN]
            if not name:
                raise ValueError("属性要有名字")
            if any(p is not prop and p["name"].lower() == name.lower() for p in schema):
                raise ValueError(f"已经有叫「{name}」的属性了")
            prop["name"] = name
        old = json.loads(json.dumps(prop))
        if op.get("type") is not None and _type(op["type"]) != prop["type"]:
            if prop["type"] == "title":
                raise ValueError("名称的类型不能改")
            new_t = _type(op["type"])
            prop["type"] = new_t
            if new_t in OPTIONED and old["type"] not in OPTIONED:
                prop["options"] = _status_default() if new_t == "status" else []
            elif new_t in OPTIONED and old["type"] in OPTIONED:   # 选项沿用；变成状态时补上分组
                prop["options"] = _clean_options(prop, old.get("options") or [], old.get("options") or [])
            elif new_t not in OPTIONED:
                prop.pop("options", None)
            if new_t == "number":
                prop["format"] = op.get("format") if op.get("format") in FORMATS else "number"
            else:
                prop.pop("format", None)
            later.append(("convert", prop["key"], old, prop))
        if op.get("options") is not None:
            if prop["type"] not in OPTIONED:
                raise ValueError(f"「{prop['name']}」不是单选、多选或状态，没有选项")
            before = {o["id"] for o in prop.get("options") or []}
            prop["options"] = _clean_options(prop, op["options"], prop.get("options") or [])
            if gone := before - {o["id"] for o in prop["options"]}:
                later.append(("unoption", prop["key"], gone, None))
        if op.get("format") is not None and prop["type"] == "number":
            if op["format"] not in FORMATS:
                raise ValueError(f"数字的格式只能是：{'、'.join(FORMATS)}")
            prop["format"] = op["format"]
    for what, key, a, b in later:
        if what == "drop":
            _rewrite_values(db_id, key, lambda v: None)
        elif what == "unoption":
            _rewrite_values(db_id, key, lambda v, gone=a: ([x for x in v if x not in gone] or None) if isinstance(v, list)
                            else None if v in gone else v)
        else:
            def conv(v, old=a, new=b):
                if new["type"] in COMPUTED:
                    return None
                if old["type"] == "multi_select" and new["type"] in ("select", "status") and isinstance(v, list):
                    v = v[:1]   # 多选变单选：留第一个
                try:
                    return normalize(new, display(old, v), create=True)
                except ValueError:
                    return None
            _rewrite_values(db_id, key, conv)
    views = [_fix_view(v, schema) for v in views] if views else _default_views(schema)
    _touch(db_id, schema=schema, views=views)
    return get_db(db_id)


# ---------- 视图 ----------
def _fix_view(v: dict, schema: list[dict]) -> dict | None:
    """表结构变了以后：去掉指向已经删掉的属性的设置；看板分组的属性没了就退回表格。"""
    keys = {p["key"]: p for p in schema}
    v = dict(v)
    v["sort"] = [s for s in v.get("sort") or [] if s.get("prop") in keys]
    v["filter"] = [f for f in v.get("filter") or [] if f.get("prop") in keys]
    v["hidden"] = [k for k in v.get("hidden") or [] if k in keys]
    v["widths"] = {k: w for k, w in (v.get("widths") or {}).items() if k in keys}
    if v.get("type") == "board" and (keys.get(v.get("group_by")) or {}).get("type") not in ("select", "status"):
        group = next((p["key"] for p in schema if p["type"] in ("status", "select")), None)
        if group:
            v["group_by"] = group
        else:
            v["type"] = "table"
            v.pop("group_by", None)
    return v


def check_views(views, schema: list[dict]) -> list[dict]:
    if not isinstance(views, list) or not views:
        raise ValueError("至少要有一个视图")
    if len(views) > MAX_VIEWS:
        raise ValueError(f"最多 {MAX_VIEWS} 个视图")
    keys = {p["key"]: p for p in schema}
    out, ids = [], set()
    for v in views:
        if not isinstance(v, dict) or v.get("type") not in ("table", "board"):
            raise ValueError("视图要是表格（table）或看板（board）")
        vid = str(v.get("id") or "") if re.fullmatch(r"[A-Za-z0-9_-]{1,20}", str(v.get("id") or "")) else _new_id("v", ids)
        if vid in ids:
            vid = _new_id("v", ids)
        ids.add(vid)
        name = " ".join(str(v.get("name") or "").split())[:40] or ("看板" if v["type"] == "board" else "表格")
        clean = {"id": vid, "name": name, "type": v["type"],
                 "sort": [{"prop": s["prop"], "desc": bool(s.get("desc"))} for s in v.get("sort") or []
                          if isinstance(s, dict) and s.get("prop") in keys][:5],
                 "filter": [_check_filter(f, keys) for f in v.get("filter") or []][:10],
                 "hidden": [k for k in v.get("hidden") or [] if k in keys and k != "title"],
                 "widths": {k: max(60, min(800, int(w))) for k, w in (v.get("widths") or {}).items()
                            if k in keys and isinstance(w, (int, float)) and not isinstance(w, bool)}}
        if v["type"] == "board":
            if (keys.get(v.get("group_by")) or {}).get("type") not in ("select", "status"):
                raise ValueError("看板要按一个单选或状态属性分列")
            clean["group_by"] = v["group_by"]
        out.append(clean)
    return out


def set_views(db_id: int, views) -> dict:
    db = get_db(db_id)
    if not db:
        raise LookupError(i18n.t("没有数据库 #{id}", id=db_id))
    _touch(db_id, views=check_views(views, db["schema"]))
    return get_db(db_id)


# ---------- 筛选、排序 ----------
def _check_filter(f, keys: dict) -> dict:
    if not isinstance(f, dict) or f.get("prop") not in keys:
        raise ValueError("筛选要指明属性")
    if f.get("op") not in OPS:
        raise ValueError(f"筛选的条件只能是：{'、'.join(OPS)}")
    out = {"prop": f["prop"], "op": f["op"]}
    if f["op"] not in ("empty", "not_empty"):
        out["value"] = f.get("value")
    return out


def _cmp_value(prop: dict, v):
    """排序、比较用的值：数字是数，选项按选项的顺序，日期、时间按字符串，其余小写的文字。"""
    t = prop["type"]
    if t == "number":
        return v
    if t in ("select", "status"):
        order = [o["id"] for o in prop.get("options") or []]
        return order.index(v) if v in order else len(order)
    if t == "date":
        return v["start"] if isinstance(v, dict) else str(v)
    if t in ("created_time", "updated_time"):
        return v
    if t == "checkbox":
        return 1 if v else 0
    return display(prop, v).lower()


def _target(prop: dict, raw):
    """筛选里给的值 → 和存的值比的样子（不自动加选项）。"""
    t = prop["type"]
    if t in ("select", "status", "multi_select"):
        return {_option(prop, x, create=False) for x in (raw if isinstance(raw, list) else [raw])}
    if t in ("person", "created_by", "updated_by"):
        return set(normalize({**prop, "type": "person"}, raw) or [])
    if t == "date" or t in ("created_time", "updated_time"):
        d = _day(raw.get("start") if isinstance(raw, dict) else raw)
        if not d:
            raise ValueError(f"「{prop['name']}」的日期写成 2026-11-15，不是「{raw}」")
        return d
    if t == "number":
        return normalize(prop, raw)
    if t == "checkbox":
        b = _truthy(raw)
        if b is None:
            raise ValueError(f"「{prop['name']}」要写 true 或 false")
        return b
    return str(raw if raw is not None else "").lower()


def _match(prop: dict, v, op: str, target) -> bool:
    t = prop["type"]
    empty = v is None or v == "" or v == [] or (t == "checkbox" and not v)
    if op == "empty":
        return empty
    if op == "not_empty":
        return not empty
    if t == "checkbox":
        return bool(v) == target if op in ("eq", "contains") else bool(v) != target
    if t in ("select", "status", "multi_select", "person", "created_by", "updated_by"):
        have = set(v if isinstance(v, list) else [v]) if not empty else set()
        hit = bool(have & target)
        return hit if op in ("eq", "contains") else not hit if op in ("neq", "not_contains") else False
    if t in ("date", "created_time", "updated_time"):
        if empty:
            return op in ("neq", "not_contains")
        cur = (v["start"] if isinstance(v, dict) else time.strftime("%Y-%m-%d %H:%M", time.localtime(v)) if isinstance(v, (int, float)) else str(v))[:len(target)]
        return {"eq": cur == target, "contains": cur == target, "neq": cur != target, "not_contains": cur != target,
                "gt": cur > target, "gte": cur >= target, "lt": cur < target, "lte": cur <= target}[op]
    if t == "number":
        if empty:
            return op in ("neq", "not_contains")
        return {"eq": v == target, "contains": v == target, "neq": v != target, "not_contains": v != target,
                "gt": v > target, "gte": v >= target, "lt": v < target, "lte": v <= target}[op]
    s = display(prop, v).lower()
    return {"eq": s == target, "neq": s != target, "contains": target in s, "not_contains": target not in s,
            "gt": s > target, "gte": s >= target, "lt": s < target, "lte": s <= target}[op]


def query(db: dict, items: list[dict], filters: list[dict] | None = None, sorts: list[dict] | None = None) -> list[dict]:
    """筛选（都要满足）和排序（可以按几列；空的总排在最后）。prop 是 key 或名字。"""
    conds = []
    for f in filters or []:
        prop = prop_of(db, f.get("prop")) if isinstance(f, dict) else None
        if not prop:
            raise ValueError(f"筛选里没有属性「{f.get('prop') if isinstance(f, dict) else f}」")
        op = f.get("op") or "eq"
        if op not in OPS:
            raise ValueError(f"筛选的条件只能是：{'、'.join(OPS)}")
        conds.append((prop, op, None if op in ("empty", "not_empty") else _target(json.loads(json.dumps(prop)), f.get("value"))))
    out = [r for r in items if all(_match(p, value_of(r, p), op, tg) for p, op, tg in conds)]
    for s in reversed(sorts or []):
        prop = prop_of(db, s.get("prop")) if isinstance(s, dict) else None
        if not prop:
            raise ValueError(f"排序里没有属性「{s.get('prop') if isinstance(s, dict) else s}」")
        has = [r for r in out if value_of(r, prop) not in (None, "", [])]
        none = [r for r in out if value_of(r, prop) in (None, "", [])]
        has.sort(key=lambda r: _cmp_value(prop, value_of(r, prop)), reverse=bool(s.get("desc")))
        out = has + none
    return out


def view_rows(db: dict, view_id: str | None = None) -> tuple[dict, list[dict]]:
    """按视图筛选、排序后的行。没给或者找不到就用第一个视图。"""
    views = db["views"] or _default_views(db["schema"])
    view = next((v for v in views if v["id"] == view_id), views[0])
    try:
        return view, query(db, rows(db["id"]), view.get("filter"), view.get("sort"))
    except ValueError:   # 视图里的筛选值已经不成立了（比如选项被删）：不筛
        return view, rows(db["id"])


# ---------- 行 ----------
def _check_props(db: dict, given: dict, row: dict | None) -> tuple[dict, bool, str | None]:
    """给的属性 {key 或名字: 值} → ({key: 存的值或 None}, 表结构有没有变（加了选项）, 新标题或 None)。"""
    if not isinstance(given, dict):
        raise ValueError("props 要是一个对象：{属性名: 值}")
    changes, title = {}, None
    before = json.dumps(db["schema"], ensure_ascii=False)
    for ref, v in given.items():
        prop = prop_of(db, ref)
        if not prop:
            raise ValueError(f"没有属性「{ref}」，现在有：{'、'.join(p['name'] for p in db['schema'])}")
        if prop["type"] == "title":
            title = " ".join(str(v or "").split())[:120]
            continue
        changes[prop["key"]] = normalize(prop, v)
    return changes, json.dumps(db["schema"], ensure_ascii=False) != before, title


def add_row(db_id: int, props: dict | None, by: str, title: str = "", content: str = "", bot_id: int | None = None,
            after: int | None = None) -> dict:
    """加一行。props 是 {key 或名字: 值}（名称也能写在里面）。after：排在哪一行后面（0 是最前，None 是最后）。"""
    db = get_db(db_id)
    if not db:
        raise LookupError(i18n.t("没有数据库 #{id}", id=db_id))
    if conn.execute("select count(*) from pages where parent_id=? and kind='row'", (db_id,)).fetchone()[0] >= MAX_ROWS:
        raise ValueError(i18n.t("一个数据库最多 {n} 行", n=MAX_ROWS))
    changes, schema_changed, t2 = _check_props(db, props or {}, None)
    if len(content or "") > library.MAX_PAGE:
        raise ValueError(i18n.t("页面太大（最多 {n}K 字）", n=library.MAX_PAGE // 1000))
    stored = {k: v for k, v in changes.items() if v is not None}
    if schema_changed:
        _touch(db_id, schema=db["schema"])
    now = time.time()
    p = library.save_page(bot_id, t2 if t2 is not None else title, content or "", "markdown", by=by, allow_empty=True,
                          parent_id=db_id, kind="row", props=stored)
    conn.execute("update pages set props_meta=? where id=?",
                 (json.dumps({k: {"v": 1, "by": by, "ts": now} for k in stored}, ensure_ascii=False), p["id"]))
    conn.commit()
    if after is not None:
        library.move(p["id"], db_id, after)
    _touch(db_id)
    return get_row(p["id"])


def update_row(row_id: int, props: dict, by: str, base: int | None = None) -> tuple[dict, list[str]]:
    """改一行的属性（名称也能改）。base：调用方读到的版本号，给了就按键查冲突。返回 (这一行, 改了哪些键)；什么都没变不涨版本。"""
    row = get_row(row_id)
    if not row:
        raise LookupError(i18n.t("没有这一行 #{id}", id=row_id))
    db = get_db(row["db_id"])
    if base is not None and (not isinstance(base, int) or isinstance(base, bool) or base < 0 or base > row["version"]):
        raise ValueError(i18n.t("版本号不对：页面现在是版本 {v}", v=row["version"]))
    changes, schema_changed, title = _check_props(db, props, row)
    cur = dict(row["props"])
    meta = dict(row["meta"])
    changed = [k for k, v in changes.items() if cur.get(k) != v]
    if title is not None and title != row["title"]:
        changed.append("title")
    if base is not None:
        stale = [k for k in changed if (meta.get(k) or {}).get("v", 0) > base and (meta.get(k) or {}).get("by") != by]
        if stale:
            keys = {p["key"]: p for p in db["schema"]}
            raise Conflict(row["version"], [{"prop": k, "name": keys[k]["name"] if k in keys else k,
                                             "current": display(keys[k], value_of(row, keys[k])) if k in keys else "",
                                             "by": meta[k].get("by", "")} for k in stale])
    if schema_changed:
        _touch(db["id"], schema=db["schema"])
    if not changed:
        return row, []
    p, _ = pagedoc.load(row_id)
    now = time.time()
    for k in changed:
        if k != "title":
            if changes[k] is None:
                cur.pop(k, None)
            else:
                cur[k] = changes[k]
        meta[k] = {"v": row["version"] + 1, "by": by, "ts": now}
    try:
        pagedoc.commit_props(p, title if "title" in changed else row["title"], cur, meta, by)
    except pagedoc.Conflict:   # 正好被别人提交了一版：重新来一次（按键查冲突）
        return update_row(row_id, props, by, base)
    _touch(db["id"])
    return get_row(row_id), changed


def restore_props(row_id: int, props: dict, by: str):
    """回到某一版时（pagedoc.restore）：属性换成那一版的，改了的键记成这次改的。版本号由接下来的提交来涨。"""
    row = get_row(row_id)
    if not row:
        return
    meta = dict(row["meta"])
    for k in set(props) | set(row["props"]):
        if props.get(k) != row["props"].get(k):
            meta[k] = {"v": row["version"] + 1, "by": by, "ts": time.time()}
    conn.execute("update pages set props=?, props_meta=? where id=?",
                 (json.dumps(props, ensure_ascii=False), json.dumps(meta, ensure_ascii=False), row_id))


def public_row(r: dict) -> dict:
    return {k: r[k] for k in ("id", "title", "version", "pos", "created", "updated", "created_by", "updated_by", "props", "locked")}


def public(db: dict, view_id: str | None = None) -> dict:
    """网页用：库、表结构、视图、按视图筛选排序后的行。"""
    view, items = view_rows(db, view_id)
    return {"id": db["id"], "title": db["title"], "locked": db["locked"], "version": db["version"], "schema": db["schema"],
            "views": db["views"], "view": view["id"], "total": conn.execute(
                "select count(*) from pages where parent_id=? and kind='row'", (db["id"],)).fetchone()[0],
            "rows": [public_row(r) for r in items]}


# ---------- 给模型看的文字 ----------
def schema_text(db: dict) -> str:
    parts = []
    for p in db["schema"]:
        t = TYPES[p["type"]]
        if p["type"] in OPTIONED:
            t += "：" + (" / ".join(o["name"] for o in p.get("options") or []) or "还没有选项")
        elif p["type"] == "number" and (p.get("format") or "number") != "number":
            t += f"（{p['format']}）"
        parts.append(f"{p['name']}（{t}）")
    return "、".join(parts)


def _cell(s: str, width: int = 80) -> str:
    s = " ".join(str(s).split()).replace("|", "\\|")
    return s if len(s) <= width else s[:width - 1] + "…"


def table_text(db: dict, items: list[dict], limit: int = 50, cols: list[dict] | None = None) -> str:
    """紧凑的 Markdown 表：行号、版本、每个属性一列（cols 只要这几列，名称总在）。"""
    cols = cols or db["schema"]
    lines = ["| 行 | 版本 | " + " | ".join(_cell(p["name"], 30) for p in cols) + " |", "|" + " --- |" * (len(cols) + 2)]
    for r in items[:limit]:
        lines.append(f"| #{r['id']} | {r['version']} | " + " | ".join(_cell(display(p, value_of(r, p))) for p in cols) + " |")
    more = f"\n…还有 {len(items) - limit} 行没显示（用 db_query 的 filter / limit 缩小或调大）" if len(items) > limit else ""
    return "\n".join(lines) + more


def render_db(db_id: int, limit_chars: int = 12000) -> str:
    """read_page 读库：库的信息、表结构、第一个视图的前面几十行。"""
    db = get_db(db_id)
    if not db:
        return f"没有数据库 #{db_id}"
    view, items = view_rows(db)
    total = len(rows(db_id))
    head = (f"数据库《{db['title']}》{library.link(db_id)} · {total} 行 · {time.strftime('%Y-%m-%d', time.localtime(db['updated']))} 更新"
            + (" · 已锁定（只有用户能改，Bot 只能读）" if db["locked"] else ""))
    views = "视图：" + "、".join(f"{v['name']}（{'看板' if v['type'] == 'board' else '表格'}）" for v in db["views"])
    body = table_text(db, items, 30)
    if len(body) > limit_chars - 600:
        body = body[:limit_chars - 700] + "\n…（太长，截断了，用 db_query 只取要的列和行）"
    hint = ("（每行是一个页面：读某一行的正文用 read_page(行号)，改正文用 edit_page；改属性用 db_update（带版本），"
            "加行用 db_add，筛选、排序用 db_query，改表结构用 db_schema）")
    return f"{head}\n属性：{schema_text(db)}\n{views}\n按「{view['name']}」显示：\n{body}\n{hint}"


def row_text(row_id: int) -> str:
    """read_page 读一行：它在哪个库、每个属性的值。"""
    row = get_row(row_id)
    if not row:
        return ""
    db = get_db(row["db_id"])
    vals = [f"{p['name']}：{display(p, value_of(row, p)) or '（空）'}" for p in db["schema"] if p["type"] != "title"]
    return (f"数据库《{db['title']}》#{db['id']} 的一行" + (" · 库已锁定（只能读）" if db["locked"] else "")
            + "\n属性（改用 db_update，带这个版本号）：" + " · ".join(vals))


def conflict_text(e: Conflict, head: str = "没改") -> str:
    lines = [f"{head}：这一行现在是版本 {e.version}，你要改的属性在你读过之后被别人改了："]
    lines += [f"「{c['name']}」被「{person_name(c['by'])}」改成了：{c['current'] or '（空）'}" for c in e.items]
    lines.append(f"按现在的值决定要不要改，用 version={e.version} 再提交。")
    return "\n".join(lines)


# ---------- 工具：Bot 和外部 Agent 共用（builtin.py、agentapi.py 包一层发事件） ----------
_TYPE_ENUM = [k for k in TYPES if k != "title"]
_VALUES = ("值的写法：文本、网址、邮箱、电话写文字；数字写数；单选写选项名（没有的会自动加上）；多选写选项名的列表；"
           "状态只能写已有的选项名；日期写 2026-11-15 或 2026-11-15 14:00，有结束日期写 {\"start\": …, \"end\": …}；"
           "人员写名字的列表（“用户”或者 Bot、外部 Agent 的名字）；勾选写 true / false；写 null 是清空。自动算的（创建时间等）不能写。")
_PROP_SPEC = {"type": "object", "properties": {
    "name": {"type": "string"},
    "type": {"type": "string", "enum": _TYPE_ENUM, "description": "、".join(f"{k} {v}" for k, v in TYPES.items() if k != "title")},
    "options": {"type": "array", "items": {"type": "string"}, "description": "单选、多选、状态的选项（按顺序）；状态不给就是 未开始 / 进行中 / 已完成"},
    "format": {"type": "string", "enum": list(FORMATS), "description": "数字的显示格式：number、comma 千分位、percent 百分比（存 50 显示 50%）、cny、usd"}},
    "required": ["name", "type"]}

DB_CREATE = {"type": "function", "function": {
    "name": "db_create",
    "description": "在资料库里建一个数据库（多维表：任务看板、会议列表、客户名单这类一条一行、每条带几个属性的东西），返回库的编号。"
                   "每一行也是一个页面，能写正文。网页上有表格和看板（按第一个状态或单选属性分列）两个视图。建好以后用 db_add 加行。",
    "parameters": {"type": "object", "properties": {
        "title": {"type": "string"},
        "properties": {"type": "array", "items": _PROP_SPEC,
                       "description": "名称以外的属性。不给就是：状态（未开始 / 进行中 / 已完成）、负责人（人员）、日期"},
        "title_name": {"type": "string", "description": "可选：名称这一列叫什么（默认“名称”）"},
        "parent_id": {"type": "integer", "description": "可选：放在哪个页面下面"}},
        "required": ["title"]}}}

DB_SCHEMA = {"type": "function", "function": {
    "name": "db_schema",
    "description": "改数据库的表结构：加属性、改名、改类型（能转的值会转过去，转不了的清空）、改选项（给全部选项，同名的保留，去掉的选项从各行清掉）、删属性。"
                   "按顺序做，有一个不行整批不做。",
    "parameters": {"type": "object", "properties": {
        "db_id": {"type": "integer"},
        "ops": {"type": "array", "items": {"type": "object", "properties": {
            "op": {"type": "string", "enum": ["add", "update", "delete"]},
            "prop": {"type": "string", "description": "update / delete：哪个属性（名字）"},
            "name": {"type": "string", "description": "add：属性名；update：新名字"},
            "type": {"type": "string", "enum": _TYPE_ENUM},
            "options": {"type": "array", "items": {"type": "string"}},
            "format": {"type": "string", "enum": list(FORMATS)}},
            "required": ["op"]}}},
        "required": ["db_id", "ops"]}}}

DB_QUERY = {"type": "function", "function": {
    "name": "db_query",
    "description": "查数据库里的行：能筛选（条件都要满足）、排序、只要几列，返回表格（每行带行号和版本号，改属性用 db_update 时要带版本号）。",
    "parameters": {"type": "object", "properties": {
        "db_id": {"type": "integer"},
        "filter": {"type": "array", "items": {"type": "object", "properties": {
            "prop": {"type": "string", "description": "属性名"},
            "op": {"type": "string", "enum": list(OPS), "description": "eq 等于、neq 不等于、contains 包含（多选、人员是含有其中一个）、"
                                                                     "not_contains、gt / gte / lt / lte 比大小（日期按先后）、empty 是空的、not_empty"},
            "value": {"description": "比的值，写法和 db_add 一样；日期能写 today / 今天 / 明天"}},
            "required": ["prop", "op"]}},
        "sort": {"type": "array", "items": {"type": "object", "properties": {
            "prop": {"type": "string"}, "desc": {"type": "boolean"}}, "required": ["prop"]}},
        "view": {"type": "string", "description": "可选：用这个视图（名字）的筛选和排序"},
        "props": {"type": "array", "items": {"type": "string"}, "description": "可选：只要这几列（名称总在）"},
        "limit": {"type": "integer", "description": "最多几行，默认 50，最多 200"}},
        "required": ["db_id"]}}}

DB_ADD = {"type": "function", "function": {
    "name": "db_add",
    "description": "往数据库里加行（一次最多 50 行），每行是 {title, props, content}。props 是 {属性名: 值}。" + _VALUES,
    "parameters": {"type": "object", "properties": {
        "db_id": {"type": "integer"},
        "rows": {"type": "array", "items": {"type": "object", "properties": {
            "title": {"type": "string", "description": "名称"},
            "props": {"type": "object", "description": "{属性名: 值}"},
            "content": {"type": "string", "description": "可选：这一行的正文（Markdown）"}}}}},
        "required": ["db_id", "rows"]}}}

DB_UPDATE = {"type": "function", "function": {
    "name": "db_update",
    "description": "改数据库里几行的属性（名称也能改，写在 props 里），一次最多 50 行，每行单独生效。带上 db_query / read_page 看到的版本号时，"
                   "你要改的属性在那之后被别人改了就不改那一行，告诉你现在的值。只改写到的属性，别的不动。改正文用 edit_page。" + _VALUES,
    "parameters": {"type": "object", "properties": {
        "rows": {"type": "array", "items": {"type": "object", "properties": {
            "row_id": {"type": "integer"},
            "props": {"type": "object", "description": "{属性名: 新值}"},
            "version": {"type": "integer", "description": "这一行的版本号"}},
            "required": ["row_id", "props"]}}},
        "required": ["rows"]}}}

TOOLS = (DB_CREATE, DB_SCHEMA, DB_QUERY, DB_ADD, DB_UPDATE)


def _who(who) -> tuple[str, int | None]:
    return (who, None) if isinstance(who, str) else (f"bot:{who}", who)


def _writable(db_id) -> tuple[dict | None, str]:
    db = get_db(db_id)
    if not db:
        p = library.get_page(db_id, False) if isinstance(db_id, int) and not isinstance(db_id, bool) else None
        return None, (f"#{db_id} 不是数据库" + ("（是数据库里的一行，库是 #" + str(p["parent_id"]) + "）" if p and p["kind"] == "row" else "")) if p else f"没有数据库 #{db_id}"
    if db["locked"]:
        return None, f"数据库 #{db_id}「{db['title']}」被用户锁定了，只能读"
    return db, ""


def tool_create(who, args: dict) -> tuple[str, dict | None]:
    """返回 (给模型的结果, 事件 {db_id} 或 None)。"""
    by, bot_id = _who(who)
    props = args.get("properties")
    try:
        if props is None:
            schema = default_schema()
            if args.get("title_name"):
                schema[0]["name"] = " ".join(str(args["title_name"]).split())[:NAME_LEN] or "名称"
            db = create(args.get("title") or "", by, schema=schema, parent_id=args.get("parent_id"), bot_id=bot_id)
        else:
            if not isinstance(props, list):
                return "没建：properties 要是一个列表", None
            db = create(args.get("title") or "", by, parent_id=args.get("parent_id"), bot_id=bot_id, properties=props,
                        title_name=args.get("title_name"))
    except (ValueError, TypeError, LookupError) as e:
        return f"没建：{e}", None
    return (f"已建数据库 #{db['id']}「{db['title']}」，链接 {library.link(db['id'])}。属性：{schema_text(db)}。"
            "用 db_add 加行；回复里用 Markdown 链接附上它。"), {"db_id": db["id"]}


def tool_schema(who, args: dict) -> tuple[str, dict | None]:
    db, why = _writable(args.get("db_id"))
    if not db:
        return f"没改：{why}", None
    try:
        db = change_schema(db["id"], args.get("ops"))
    except (ValueError, TypeError, LookupError) as e:
        return f"没改：{e}", None
    return f"已改数据库 #{db['id']}「{db['title']}」的表结构，现在的属性：{schema_text(db)}", {"db_id": db["id"]}


def tool_query(args: dict) -> str:
    db = get_db(args.get("db_id"))
    if not db:
        return _writable(args.get("db_id"))[1]
    try:
        items = rows(db["id"])
        total = len(items)
        filters, sorts = args.get("filter") or [], args.get("sort") or []
        if args.get("view"):
            view = next((v for v in db["views"] if v["name"] == str(args["view"]).strip() or v["id"] == args["view"]), None)
            if not view:
                return f"没有视图「{args['view']}」，有：{'、'.join(v['name'] for v in db['views'])}"
            filters, sorts = list(view.get("filter") or []) + list(filters), list(sorts) or list(view.get("sort") or [])
        if not isinstance(filters, list) or not isinstance(sorts, list):
            return "没查：filter 和 sort 要是列表"
        items = query(db, items, filters, sorts)
        cols = None
        if args.get("props"):
            want = [prop_of(db, x) for x in args["props"]]
            if None in want:
                return f"没查：没有属性「{args['props'][want.index(None)]}」，有：{'、'.join(p['name'] for p in db['schema'])}"
            cols = [db["schema"][0]] + [p for p in want if p["type"] != "title"]
    except (ValueError, TypeError) as e:
        return f"没查：{e}"
    limit = args.get("limit") if isinstance(args.get("limit"), int) and not isinstance(args.get("limit"), bool) else 50
    limit = max(1, min(200, limit))
    head = f"数据库 #{db['id']}「{db['title']}」共 {total} 行" + (f"，符合条件的 {len(items)} 行" if filters else "") + (
        "（已锁定，只能读）" if db["locked"] else "")
    if not items:
        return head + "，没有符合条件的行。" if total else head + "，还没有行。"
    return f"{head}：\n{table_text(db, items, limit, cols)}"


def tool_add(who, args: dict) -> tuple[str, dict | None]:
    by, bot_id = _who(who)
    db, why = _writable(args.get("db_id"))
    if not db:
        return f"没加：{why}", None
    items = args.get("rows")
    if not isinstance(items, list) or not items:
        return "没加：rows 要是一个列表，每个是 {title, props, content}", None
    if len(items) > 50:
        return "没加：一次最多 50 行", None
    made, errors = [], []
    for k, it in enumerate(items, 1):
        if not isinstance(it, dict):
            errors.append(f"第 {k} 行：要是一个对象")
            continue
        try:
            r = add_row(db["id"], it.get("props") or {}, by, str(it.get("title") or ""), str(it.get("content") or ""), bot_id)
            made.append(r)
        except (ValueError, TypeError, LookupError) as e:
            errors.append(f"第 {k} 行没加：{e}")
    out = []
    if made:
        out.append(f"已在数据库 #{db['id']}「{db['title']}」加了 {len(made)} 行：" + "、".join(f"#{r['id']} {r['title']}" for r in made) + "（版本都是 1）")
    out += errors
    return "\n".join(out), ({"db_id": db["id"], "rows": [r["id"] for r in made]} if made else None)


def tool_update(who, args: dict) -> tuple[str, dict | None]:
    by, _ = _who(who)
    items = args.get("rows")
    if not isinstance(items, list) or not items:
        return "没改：rows 要是一个列表，每个是 {row_id, props, version}", None
    if len(items) > 50:
        return "没改：一次最多 50 行", None
    out, touched, dbs_ = [], [], set()
    for it in items:
        rid = it.get("row_id") if isinstance(it, dict) else None
        row = get_row(rid)
        if not row:
            out.append(f"#{rid}：没有这一行（行号是 db_query 里的 #数字）")
            continue
        db, why = _writable(row["db_id"])
        if not db:
            out.append(f"#{rid}：没改，{why}")
            continue
        if row["locked"]:
            out.append(f"#{rid}：没改，这一行被用户锁定了")
            continue
        try:
            new, changed = update_row(rid, it.get("props") or {}, by, it.get("version"))
        except Conflict as e:
            out.append(f"#{rid}：" + conflict_text(e))
            continue
        except (ValueError, TypeError, LookupError) as e:
            out.append(f"#{rid}：没改，{e}")
            continue
        if changed:
            touched.append(rid)
            dbs_.add(row["db_id"])
            keys = {p["key"]: p["name"] for p in db["schema"]}
            out.append(f"#{rid}「{new['title']}」改了 {'、'.join(keys.get(k, k) for k in changed)}，现在是版本 {new['version']}")
        else:
            out.append(f"#{rid}「{new['title']}」没有变化（版本 {new['version']}）")
    ev = {"db_id": next(iter(dbs_)), "rows": touched} if len(dbs_) == 1 else {"db_ids": sorted(dbs_), "rows": touched} if dbs_ else None
    return "\n".join(out), ev


def index_text(pid: int) -> str:
    """搜索索引用的文字：行是“属性名：值”，库是它的属性名。"""
    row = get_row(pid)
    if row:
        db = get_db(row["db_id"])
        return "\n".join(f"{p['name']}：{v}" for p in db["schema"] if p["type"] != "title" and p["type"] not in COMPUTED
                         and (v := display(p, value_of(row, p))))
    db = get_db(pid)
    return ("数据库，属性：" + "、".join(p["name"] for p in db["schema"])) if db else ""


async def announce(ev: dict | None, by: str, created: bool = False):
    """工具改了库以后发事件：开着的库重新取（db_changed），新建的库、行的标题变了资料库列表也刷新（pages_changed）。"""
    if not ev:
        return
    from . import registry
    for db_id in ev.get("db_ids") or [ev["db_id"]]:
        await registry.emit("db_changed", db_id=db_id, rows=ev.get("rows") or [], by=by)
    await registry.emit("pages_changed")


def share_markdown(db_id: int) -> str:
    """分享出去的库：第一个视图筛选、排序后的表格（隐藏的列不显示）。"""
    db = get_db(db_id)
    if not db:
        return ""
    view, items = view_rows(db)
    cols = [p for p in db["schema"] if p["key"] not in (view.get("hidden") or [])]

    def cell(s: str) -> str:
        return " ".join(str(s).split()).replace("|", "\\|") or " "
    lines = ["| " + " | ".join(cell(p["name"]) for p in cols) + " |", "|" + " --- |" * len(cols)]
    lines += ["| " + " | ".join(cell(display(p, value_of(r, p))) for p in cols) + " |" for r in items]
    return "\n".join(lines) if items else "\n".join(lines) + "\n\n（还没有行）"
