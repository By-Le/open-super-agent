"""从 Notion 导入（osa/notion_import.py）。"""
import asyncio
import json

import httpx
from osa import mcp_hub, notion_import, server


def http():
    return httpx.AsyncClient(transport=httpx.ASGITransport(app=server.app), base_url="http://127.0.0.1")


class FakeResult:
    def __init__(self, text, error=False):
        self.content = [type("C", (), {"text": text})()]
        self.is_error = error


class FakeSession:
    def __init__(self):
        self.calls = []

    async def call_tool(self, tool, args):
        self.calls.append((tool, args))
        return FakeResult("坏了", True) if args.get("bad") else FakeResult(f"{tool}:{args}")


async def test_read_only_calls(monkeypatch):
    fake = FakeSession()
    async with http() as c:
        monkeypatch.setattr(mcp_hub, "_sessions", {})
        assert (await c.post("/api/notion/read", json={"tool": "notion-search", "args": {"query": "x"}})).status_code == 409
        monkeypatch.setattr(mcp_hub, "_sessions", {"notion": fake})
        r = await c.post("/api/notion/read", json={"tool": "notion-search", "args": {"query": "公司"}})
        assert r.status_code == 200 and r.json()["text"].startswith("notion-search")
        assert (await c.post("/api/notion/read", json={"tool": "notion-update-page", "args": {}})).status_code == 400   # 不让写
        assert (await c.post("/api/notion/read", json={"tool": "notion-fetch", "args": {"bad": 1}})).status_code == 502
    assert [t for t, _ in fake.calls] == ["notion-search", "notion-fetch"]


# ---------- 转换 ----------
def test_to_markdown():
    from osa.notion_import import to_markdown
    links = {"a" * 32: "/library/pages/7"}
    link = lambda url: links.get(notion_import.nid(url))   # noqa: E731
    src = "\n".join([
        "开头 <b>唯一账本 = 任务卡</b>（见下）；<b>「已发布」</b>的才算；<i>斜体</i>、<s>划掉</s>、<span color=\"red\">红字</span>",
        "调用：<code>codex exec --add-dir <skill目录> \"<提示词>\" </dev/null</code> 和 <code>a`b</code>",
        f'见 <mention url="https://app.notion.com/p/{"a" * 32}" type="database">任务卡</mention>，外面的 <mention url="https://app.notion.com/p/{"b" * 32}">别处</mention>',
        f"原文链接 https://app.notion.com/p/{'a' * 32} 和 [文](https://www.notion.so/x-{'a' * 32})",
        '日期 <date start="2026-04-24" start-time="11:35" time-zone="Asia/Shanghai" /> · <mention-user url="user://1">Kada</mention-user>',
        f'总页见 <mention url="https://app.notion.com/p/{"a" * 32}" />，另见 <mention url="https://app.notion.com/p/{"b" * 32}">别处二</mention>。',
        "提示词：0–5s: <wooden door creaking open> <birds>，主角来自 <<<image_1>>>，a < b",
        "## 标题 {color=\"blue\"}",
        '<callout icon="⚠️" color="yellow_bg">',
        "<b>结论</b>：注意",
        "</callout>",
        "<details>", "<summary>展开看</summary>", "\t里面一行", "</details>",
        '<table header-row="true">', "<tr><td>名</td><td>值|竖线</td></tr>", "<tr><td>a</td><td>1</td></tr>", "</table>",
        "<ul>", "<li></li>", "</ul>", "<empty-block/>",
        "```python", "x = '<b>不动</b>'", "```",
        f'<page url="https://app.notion.com/p/{"a" * 32}">', "子页面", "</page>",
        f'<database url="{{{{https://app.notion.com/p/{"c" * 32}}}}}" inline="true">', "内嵌库", "</database>",
    ])
    md = to_markdown(src, link, lambda url: "任务卡库" if notion_import.nid(url) == "a" * 32 else None)
    assert "**唯一账本 = 任务卡**（见下）" in md
    assert "； **「已发布」** 的才算" in md or "；**「已发布」** 的才算" in md   # 标点挨着时补空格，不然认不出粗体
    assert "*斜体*、~~划掉~~、红字" in md
    assert '`codex exec --add-dir <skill目录> "<提示词>" </dev/null`' in md and "``a`b``" in md
    assert "[任务卡](/library/pages/7)" in md and f"[别处](https://app.notion.com/p/{'b' * 32})" in md
    assert "原文链接 [任务卡库](/library/pages/7) 和 [文](/library/pages/7)" in md
    assert "日期 2026-04-24 11:35 · @Kada" in md and "## 标题\n" in md
    assert f"总页见 [任务卡库](/library/pages/7)，另见 [别处二](https://app.notion.com/p/{'b' * 32})。" in md   # 没写标题的提及不吞掉后面的
    assert "0–5s: \\<wooden door creaking open> \\<birds>，主角来自 \\<\\<\\<image_1>>>，a < b" in md
    assert "> [!WARNING]\n> **结论**：注意" in md
    assert "<details><summary>展开看</summary>\n\n里面一行\n\n</details>" in md
    assert "| 名 | 值\\|竖线 |\n| --- | --- |\n| a | 1 |" in md
    assert "<ul>" not in md and "<empty-block" not in md and "x = '<b>不动</b>'" in md
    assert notion_import.parse_fetch(json.dumps({"title": "🗄 🗄 归档", "text": '<page url="https://app.notion.com/p/x" icon="🗄">\n<properties>\n{"title":"🗄 归档"}\n</properties>\n<content>\n</content>\n</page>'}))["title"] == "🗄 归档"
    assert notion_import.parse_fetch(json.dumps({"title": "x", "text": '<page url="https://app.notion.com/p/x" icon="🏢">\n<properties>\n{"title":"总部"}\n</properties>\n<content>\n</content>\n</page>'}))["title"] == "🏢 总部"
    assert "📄 [子页面](/library/pages/7)" in md and f"▦ [内嵌库](https://app.notion.com/p/{'c' * 32})" in md


# ---------- 整个导入（假的 Notion） ----------
ROOT, CHILD, DB, VIEW, ARCH = "1" * 32, "2" * 32, "3" * 32, "4" * 32, "5" * 32
R1, R2 = "6" * 32, "7" * 32
SRC = "collection://00000000-1111-2222-3333-444444444444"


def page(i, title, content, props=None):
    body = f'<page url="https://app.notion.com/p/{i}">\n<properties>\n{json.dumps(props or {"title": title}, ensure_ascii=False)}\n</properties>\n<content>\n{content}\n</content>\n</page>'
    return json.dumps({"metadata": {"type": "page"}, "title": title, "url": f"https://app.notion.com/p/{i}", "text": "Here is the result:\n" + body},
                      ensure_ascii=False)


def database(i, title, src, views=None):
    schema = {"标题": {"type": "title", "name": "标题"}, "编号": {"type": "auto_increment_id", "name": "编号"},
              "状态": {"type": "status", "name": "状态", "groups": {"to_do": [{"name": "待办", "color": "gray"}],
                                                                  "in_progress": [{"name": "进行中", "color": "blue"}],
                                                                  "complete": [{"name": "已发布", "color": "green"}]}},
              "接手人": {"type": "select", "name": "接手人", "options": [{"name": "Nova", "color": "blue"}, {"name": "Forge", "color": "orange_background"}]},
              "截止日期": {"type": "date", "name": "截止日期"}, "要": {"type": "checkbox", "name": "要"}, "分": {"type": "number", "name": "分"},
              "产出位置": {"type": "text", "name": "产出位置"}, "人": {"type": "person", "name": "人"}, "封面": {"type": "file", "name": "封面"}}
    state = json.dumps({"name": title, "schema": schema}, ensure_ascii=False)
    text = (f'<database url="{{{{https://app.notion.com/p/{i}}}}}" inline="false">\nThe title of this Database is: {title}\n<data-sources>\n'
            f'<data-source url="{{{{{src}}}}}">\n<data-source-state>\n{state}\n</data-source-state>\n</data-source>\n</data-sources>\n'
            '<views>\n' + "".join(f'<view url="{{{{{u}}}}}">\n{json.dumps(v, ensure_ascii=False)}\n</view>\n' for u, v in views or [
                ("view://f", {"type": "table", "filter": {"property": "状态", "exact": "待办"}}),   # 带筛选的不能用来查全部的行
                ("view://x", {"displayProperties": ["标题", "编号", "状态", "接手人", "截止日期"], "type": "table"})]) + '</views>\n</database>')
    return json.dumps({"metadata": {"type": "database"}, "title": title, "url": f"https://app.notion.com/p/{i}", "text": text}, ensure_ascii=False)


class FakeNotion:
    def __init__(self):
        self.status = {R1: "进行中", R2: "已发布"}
        self.root_extra = ""

    async def __call__(self, tool, args, timeout=60):
        if tool == "notion-get-users":
            return json.dumps({"results": [{"id": "user://abc", "name": "Kada"}]})
        if tool == "notion-query-data-sources":
            assert args["data"]["mode"] == "view" and args["data"]["view_url"] == "view://x"   # 按视图查，不用 SQL（免费版有限额）
            if not args["data"].get("start_cursor"):   # 分两页给
                return json.dumps({"results": [], "has_more": True, "next_cursor": "c2"})
            assert args["data"]["start_cursor"] == "c2"
            rows = [{"url": f"https://app.notion.com/{R2}", "createdTime": "2026-10-14 07:23:25Z", "标题": "写周报", "编号": "7", "状态": self.status[R2],
                     "接手人": "Forge", "date:截止日期:start": "2026-11-20", "date:截止日期:end": None, "date:截止日期:is_datetime": 0,
                     "要": "__YES__", "分": "3", "产出位置": f'见 <mention url="https://app.notion.com/p/{R1}">做设计</mention>', "人": '["user://abc"]'},
                    {"url": f"https://app.notion.com/{R1}", "createdTime": "2026-10-13 01:00:00Z", "标题": "做设计", "编号": 3, "状态": self.status[R1],
                     "接手人": "Nova", "要": "__NO__", "人": "[]", "产出位置": f'接着看 <mention url="https://app.notion.com/p/{R2}">写周报</mention>'}]
            return json.dumps({"results": rows, "has_more": False}, ensure_ascii=False)
        i = notion_import.nid(args["id"])
        if i == ROOT:
            return page(ROOT, "🏢 总部", f"账本是 <mention url=\"https://app.notion.com/p/{DB}\" type=\"database\">任务卡</mention>，看 <mention url=\"https://app.notion.com/p/{R1}\">做设计</mention>\n\n"
                                       f'<page url="https://app.notion.com/p/{CHILD}">\n驾驶舱\n</page>{self.root_extra}')
        if i == CHILD:   # 先遇到“View of”（带筛选的视图），原库在后面
            return page(CHILD, "驾驶舱", f'<database url="{{{{https://app.notion.com/p/{VIEW}}}}}" inline="true">\nView of 任务卡\n</database>\n'
                                       f'<database url="{{{{https://app.notion.com/p/{DB}}}}}" inline="true">\n任务卡\n</database>')
        if i == DB:
            return database(DB, "任务卡", SRC)
        if i == VIEW:
            return database(VIEW, "View of 任务卡", SRC, [("view://f", {"type": "gallery", "filter": {"property": "状态", "exact": "待办"}})])
        if i == ARCH:
            return page(ARCH, "归档", "旧的")
        if i == R1:
            return page(R1, "做设计", f"正文，回到 <mention url=\"https://app.notion.com/p/{ROOT}\">总部</mention>", {"编号": "TASK-3"})
        if i == R2:
            return page(R2, "写周报", "周报正文", {"编号": "TASK-7", "封面": ["https://files.example/x/cover.webp?sig=1"]})
        raise RuntimeError(f"没有 {i}")


async def wait_job(job):
    for _ in range(200):
        j = notion_import.JOBS[job]
        if j.state != "running":
            return j
        await asyncio.sleep(0.01)
    raise AssertionError("导入没结束")


async def test_import_and_sync(monkeypatch, events):
    from osa import dbs, library
    fake = FakeNotion()
    monkeypatch.setattr(notion_import, "call", fake)
    got_files = []

    async def files(self, urls):
        got_files.append(urls)
        return [{"name": "cover.webp", "url": "https://files.example/kept"}] if urls else None
    monkeypatch.setattr(notion_import.Import, "_files", files)
    monkeypatch.setattr(mcp_hub, "_sessions", {"notion": object()})
    created = []
    try:
        async with http() as c:
            r = await c.post("/api/notion/import", json={"roots": [f"https://app.notion.com/p/{ROOT}", ARCH]})
            assert r.status_code == 200
            j = await wait_job(r.json()["job"])
            assert j.state == "done", j.error
            st = (await c.get(f"/api/notion/import/{r.json()['job']}")).json()
            assert st["counts"]["pages"] == 3 and st["counts"]["dbs"] == 1 and st["counts"]["rows"] == 2
        root, child, arch, db_id = (j.nodes[x]["page_id"] for x in (ROOT, CHILD, ARCH, DB))
        created += [root, arch]
        assert library.get_page(root, False)["title"] == "🏢 总部" and library.get_page(child, False)["parent_id"] == root
        assert dbs.get_db(db_id)["parent_id"] == child and VIEW not in j.nodes   # “View of”只当链接
        rows = {r["title"]: r for r in dbs.rows(db_id)}
        r1 = rows["做设计"]
        rc = library.get_page(root)["content"]
        assert f"[任务卡](/library/pages/{db_id})" in rc and f"[做设计](/library/pages/{r1['id']})" in rc and f"📄 [驾驶舱](/library/pages/{child})" in rc
        assert f"[View of 任务卡](/library/pages/{db_id})" in library.get_page(child)["content"]
        assert f"[总部](/library/pages/{root})" in library.get_page(r1["id"])["content"]
        db = dbs.get_db(db_id)
        P = {p["name"]: p for p in db["schema"]}
        assert [p["name"] for p in db["schema"]][:5] == ["标题", "编号", "状态", "接手人", "截止日期"]   # 按 Notion 视图里的顺序
        assert [(o["name"], o["group"]) for o in P["状态"]["options"]] == [("待办", "todo"), ("进行中", "doing"), ("已发布", "done")]
        assert P["接手人"]["options"][1]["color"] == "orange" and P["编号"]["prefix"] == "TASK-" and P["编号"]["next"] == 8
        w = rows["写周报"]
        assert [r["title"] for r in dbs.rows(db_id)] == ["做设计", "写周报"]   # 按编号排
        assert w["props"][P["编号"]["key"]] == 7 and r1["props"][P["编号"]["key"]] == 3
        assert dbs.display(P["编号"], 7) == "TASK-7" and dbs.display(P["状态"], w["props"][P["状态"]["key"]]) == "已发布"
        assert w["props"][P["截止日期"]["key"]] == {"start": "2026-11-20"} and w["props"][P["要"]["key"]] is True and w["props"][P["分"]["key"]] == 3
        assert w["props"][P["人"]["key"]] == "Kada" and P["人"]["type"] == "text"
        assert w["props"][P["产出位置"]["key"]] == f"见 [做设计](/library/pages/{r1['id']})"   # 属性里的提及也换成资料库的链接
        assert r1["props"][P["产出位置"]["key"]] == f"接着看 [写周报](/library/pages/{w['id']})"   # 提到后面的行也对得上
        assert ["https://files.example/x/cover.webp?sig=1"] in got_files and w["props"][P["封面"]["key"]] == [{"name": "cover.webp", "url": "https://files.example/kept"}]
        assert library.get_page(w["id"], False)["created"] < library.get_page(db_id, False)["created"]   # 留着 Notion 里的创建时间
        # 再导一次：不重复建，改了的跟着变，没改的段编号不变
        before = [b["id"] for b in __import__("osa").pagedoc.load(root)[1]]
        fake.status[R1] = "已发布"
        fake.root_extra = "\n\n新加的一段"
        async with http() as c:
            r = await c.post("/api/notion/import", json={"roots": [ROOT]})
            j2 = await wait_job(r.json()["job"])
        assert j2.state == "done" and j2.counts["pages"] == 0 and j2.counts["rows"] == 0 and j2.counts["updated"] >= 2
        assert len(dbs.rows(db_id)) == 2 and dbs.get_row(r1["id"])["props"][P["状态"]["key"]] == P["状态"]["options"][2]["id"]
        after = [b["id"] for b in __import__("osa").pagedoc.load(root)[1]]
        assert after[:len(before)] == before and "新加的一段" in library.get_page(root)["content"]
        assert {x["page_id"] for x in notion_import.imported()} >= {root, arch}
    finally:
        for pid in created:
            if library.get_page(pid, False):
                library.delete_page(pid, cascade=True, trash=False)
