"""资料库的数据库（osa/dbs.py、web/src/components/db/）：表格里加列、新建行、改各种格子；别人改的实时出现；筛选；
看板拖动换列；删列以后在“历史”里撤销；行页面的属性栏；删行；手机宽度。不花钱（不调模型）。"""
import json
import os
import tempfile
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

BASE = os.environ.get("OSA_E2E_BASE", "http://127.0.0.1:8766")
HERE = Path(os.environ.get("OSA_E2E_OUT", tempfile.gettempdir()))   # 截图放这里


def api(path: str, body=None, method=None):
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode() if body is not None else None, method=method,
                                 headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req))


def check(ok, what):
    print(("OK   " if ok else "FAIL ") + what)
    if not ok:
        check.failed += 1
check.failed = 0


DB = api("/api/dbs", {"title": "E2E 数据库"})
ID = DB["id"]
ST = {o["name"]: o["id"] for o in DB["schema"][1]["options"]}
print("db", ID)


def rows():
    return api(f"/api/dbs/{ID}")["rows"]


def prop(name):
    return next(p for p in api(f"/api/dbs/{ID}")["schema"] if p["name"] == name)


def opt(name, option):
    return next((o["id"] for o in prop(name).get("options") or [] if o["name"] == option), None)


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 1280, "height": 860})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.on("console", lambda m: m.type == "error" and errors.append(m.text))
    pop = page.locator(".db-pop")

    def ok_dialog():
        page.locator(".dialog .modal-actions button").last.click()
        time.sleep(0.3)

    def cell(row: int, col: int):
        return page.locator(f".db-table tbody tr:nth-child({row}) td:nth-child({col}) .db-cell")

    page.goto(f"{BASE}/library/pages/{ID}")
    page.wait_for_selector(".db-table", timeout=15000)
    check([h.inner_text() for h in page.locator(".db-th").all()] == ["Aa名称", "◔状态", "◯负责人", "◷日期"] or page.locator(".db-th").count() == 4,
          "新库有名称、状态、负责人、日期四列：" + " / ".join(h.inner_text() for h in page.locator(".db-th").all()))

    # 加一列（单选）
    page.locator(".db-th-add button").click()
    pop.locator("input").fill("优先级")
    pop.locator("select").select_option("select")
    pop.locator("button", has_text="添加").click()
    page.wait_for_selector(".db-th:has-text('优先级')", timeout=5000)
    check(prop("优先级")["type"] == "select", "表头“＋”加了一列单选")

    # 新建一行：标题直接能打
    page.locator(".db-new").click()
    page.wait_for_selector(".db-td-title input", timeout=5000)
    page.keyboard.type("写周报")
    page.keyboard.press("Enter")
    time.sleep(0.5)
    check([r["title"] for r in rows()] == ["写周报"], "新建一行，标题就地打进去")

    # 状态：从选项里点
    cell(1, 2).click()
    pop.locator(".db-picker-item", has_text="进行中").click()
    time.sleep(0.5)
    check(rows()[0]["props"].get("status") == ST["进行中"], "状态格点选“进行中”")
    # 单选：打一个新的名字回车就加上这个选项
    cell(1, 5).click()
    pop.locator("input").fill("高")
    page.keyboard.press("Enter")
    time.sleep(0.6)
    check(opt("优先级", "高") is not None and rows()[0]["props"].get(prop("优先级")["key"]) == opt("优先级", "高"), "单选格里打新名字，加了选项并选上")
    # 日期、人员
    cell(1, 4).click()
    pop.locator("input[type=date]").first.fill("2026-11-20")
    time.sleep(0.5)
    page.keyboard.press("Escape")
    check(rows()[0]["props"].get("due") == {"start": "2026-11-20"}, "日期格选了 2026-11-20")
    cell(1, 3).click()
    pop.locator(".db-picker-item", has_text="你").click()
    time.sleep(0.5)
    page.keyboard.press("Escape")
    check(rows()[0]["props"].get("owner") == ["user"], "人员格选了“你”")
    page.screenshot(path=str(HERE / "40-db-table.png"))

    # 别人加的行、改的值实时出现
    other = api(f"/api/dbs/{ID}/rows", {"title": "设计评审", "props": {"status": ST["未开始"]}})
    page.wait_for_selector(".db-title-text:has-text('设计评审')", timeout=5000)
    api(f"/api/dbs/{ID}/rows/{other['id']}", {"props": {"status": ST["已完成"]}}, "PATCH")
    try:
        page.wait_for_selector(".db-table tbody tr:nth-child(2) .db-chip:has-text('已完成')", timeout=5000)
        check(True, "别人加的行、改的状态实时出现")
    except Exception as e:
        check(False, "没看到别人改的：" + str(e).splitlines()[0])

    # 筛选：状态是“进行中”
    page.locator(".db-tool", has_text="筛选").click()
    pop.locator("button", has_text="加一个条件").click()
    row = pop.locator(".db-filter-row").first
    row.locator("select").nth(0).select_option("status")
    time.sleep(0.4)
    pop.locator(".db-filter-row").first.locator("select").nth(2).select_option(ST["进行中"])
    time.sleep(0.8)
    check(page.locator(".db-table tbody tr").count() == 1 and "写周报" in page.locator(".db-table tbody").inner_text(), "筛选“状态 是 进行中”只剩一行")
    pop.locator(".db-filter-row button").first.click()
    time.sleep(0.6)
    page.keyboard.press("Escape")
    check(page.locator(".db-table tbody tr").count() == 2, "去掉条件以后两行都在")

    # 看板：拖到“已完成”
    page.locator(".db-tab", has_text="看板").click()
    page.wait_for_selector(".db-board", timeout=5000)
    card = page.locator(".db-card", has_text="写周报")
    target = page.locator(".db-col", has=page.locator(".db-col-head .db-chip", has_text="已完成"))
    card.drag_to(target)
    time.sleep(0.8)
    check(next(r for r in rows() if r["title"] == "写周报")["props"].get("status") == ST["已完成"], "看板上把卡片拖到“已完成”")
    page.screenshot(path=str(HERE / "41-db-board.png"))

    # 删一列再从“历史”撤销
    page.locator(".db-tab", has_text="表格").click()
    page.wait_for_selector(".db-table", timeout=5000)
    page.locator(".db-th", has_text="优先级").click()
    pop.locator("button", has_text="删除属性").click()
    ok_dialog()   # 确认删除
    page.wait_for_selector(".dialog", timeout=5000)
    check("清掉了 1 行的值" in page.locator(".dialog").inner_text(), "删列以后提示清掉了几行、能撤销：" + page.locator(".dialog").inner_text().replace("\n", " ")[:60])
    ok_dialog()
    check(page.locator(".db-th", has_text="优先级").count() == 0, "那一列没了")
    page.locator(".db-tool", has_text="历史").click()
    page.wait_for_selector(".db-history-item", timeout=5000)
    first = page.locator(".db-history-item").first
    check("删了属性「优先级」" in first.inner_text(), "历史里有这次删列：" + first.inner_text().replace("\n", " ")[:60])
    page.screenshot(path=str(HERE / "42-db-history.png"))
    first.locator("button", has_text="撤销").click()
    ok_dialog()
    time.sleep(0.8)
    page.keyboard.press("Escape")
    check(page.locator(".db-th", has_text="优先级").count() == 1 and next(r for r in rows() if r["title"] == "写周报")["props"].get(prop("优先级")["key"]) == opt("优先级", "高"),
          "撤销以后列和值都回来了")

    # 行页面：属性栏能改，路径能回到库
    page.locator(".db-table tbody tr", has_text="写周报").locator(".db-open").click()
    page.wait_for_selector(".db-props", timeout=10000)
    page.locator(".db-prop", has_text="状态").locator(".db-cell").click()
    pop.locator(".db-picker-item", has_text="未开始").click()
    time.sleep(0.6)
    check(next(r for r in rows() if r["title"] == "写周报")["props"].get("status") == ST["未开始"], "行页面的属性栏改了状态")
    page.screenshot(path=str(HERE / "43-db-row.png"))
    page.locator(".page-path button", has_text="E2E 数据库").click()
    page.wait_for_selector(".db-table", timeout=5000)
    check(True, "路径上点库名回到库")

    # 删一行
    page.locator(".db-table tbody tr", has_text="设计评审").hover()
    page.locator(".db-table tbody tr", has_text="设计评审").locator(".db-row-more").click()
    pop.locator("button", has_text="删除").click()
    ok_dialog()
    time.sleep(0.6)
    check([r["title"] for r in rows()] == ["写周报"], "行菜单里删了一行")

    # 手机宽度
    phone = browser.new_page(viewport={"width": 390, "height": 844}, is_mobile=True, has_touch=True)
    phone.goto(f"{BASE}/library/pages/{ID}")
    phone.wait_for_selector(".db-table", timeout=15000)
    time.sleep(0.5)
    phone.screenshot(path=str(HERE / "44-db-phone.png"))
    wide = phone.evaluate("document.documentElement.scrollWidth <= innerWidth + 1")
    check(wide, "手机宽度下页面本身不横向滚动（表格在自己的框里滑）")
    check(not errors, f"页面上没有脚本错误：{errors[:3]}")
    browser.close()

api(f"/api/pages/{ID}", method="DELETE")
print("失败", check.failed, "项")
