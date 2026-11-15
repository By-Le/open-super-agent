"""数据库的新属性类型和大库：表头加公式、双向关联（选行、对方库实时出现）、汇总、自动编号、传文件、按钮（改状态 + 写评论）；
几百行时滚到底接着加载、筛掉的行数；改视图记进历史、撤销；手机上长按卡片拖到别的列。不花钱（不调模型）。"""
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


TASK = api("/api/dbs", {"title": "E2E 任务"})
PROJ = api("/api/dbs", {"title": "E2E 项目"})
T, P = TASK["id"], PROJ["id"]
ST = {o["name"]: o["id"] for o in TASK["schema"][1]["options"]}
api(f"/api/dbs/{T}/schema", {"ops": [{"op": "add", "name": "工时", "type": "number"}]}, "PATCH")
HOURS = next(p["key"] for p in api(f"/api/dbs/{T}")["schema"] if p["name"] == "工时")
p1 = api(f"/api/dbs/{P}/rows", {"title": "官网改版"})
t1 = api(f"/api/dbs/{T}/rows", {"title": "写文案", "props": {HOURS: 3, "status": ST["进行中"]}})
t2 = api(f"/api/dbs/{T}/rows", {"title": "做设计", "props": {HOURS: 5}})
print("dbs", T, P)


def schema(db):
    return {p["name"]: p for p in api(f"/api/dbs/{db}")["schema"]}


def row(db, rid):
    return api(f"/api/dbs/{db}/rows/{rid}")["row"]


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 1400, "height": 900})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.on("console", lambda m: m.type == "error" and errors.append(m.text))
    pop = page.locator(".db-pop")
    page.goto(f"{BASE}/library/pages/{T}")
    page.wait_for_selector(".db-table", timeout=15000)

    def add_prop(name, type_, fill=None):
        page.locator(".db-th-add button").click()
        pop.locator("input").first.fill(name)
        pop.locator("select").first.select_option(type_)
        time.sleep(0.3)
        if fill:
            fill()
        pop.locator("button", has_text="添加").click()
        page.wait_for_selector(f".db-th:has-text('{name}')", timeout=5000)

    def cell(title, name):
        tr = page.locator(".db-table tbody tr", has_text=title)
        k = [h.inner_text().strip()[1:].strip() for h in page.locator(".db-th").all()].index(name)
        return tr.locator("td").nth(k).locator(".db-cell, .db-title-cell").first

    # 公式：表头加一列，写公式
    add_prop("双倍工时", "formula", lambda: pop.locator("textarea").fill('prop("工时") * 2'))
    time.sleep(0.5)
    check(cell("写文案", "双倍工时").inner_text().strip() == "6" and cell("做设计", "双倍工时").inner_text().strip() == "10", "公式列算出来了：6、10")
    cell("写文案", "工时").click()
    page.keyboard.press("ControlOrMeta+a")
    page.keyboard.type("4")
    page.keyboard.press("Enter")
    page.wait_for_function(f"""[...document.querySelectorAll('.db-table tbody tr')].some(tr => tr.innerText.includes('写文案') && tr.innerText.includes('8'))""", timeout=5000)
    check(True, "改了工时，公式跟着变成 8")

    # 双向关联：关联到项目库
    def rel_fill():
        pop.locator(".db-config select").first.select_option(str(P))
        pop.locator(".db-config input[type=checkbox]").check()
    add_prop("项目", "relation", rel_fill)
    sp = schema(P)
    check("E2E 任务" in sp and sp["E2E 任务"].get("mirror_of"), "双向：项目库里多了反向的一列「E2E 任务」")
    cell("写文案", "项目").click()
    pop.locator(".db-picker-item", has_text="官网改版").click()
    time.sleep(0.6)
    page.keyboard.press("Escape")
    check(row(T, t1["id"])["props"].get(schema(T)["项目"]["key"]) == [p1["id"]], "关联格里选了「官网改版」")
    check("官网改版" in cell("写文案", "项目").inner_text(), "格子里显示对方行的名称")
    page.screenshot(path=str(HERE / "50-db-formula-relation.png"))

    # 汇总（项目库）、自动编号、按钮、文件（任务库）用接口建，网页上看
    api(f"/api/dbs/{P}/schema", {"ops": [{"op": "add", "name": "总工时", "type": "rollup", "relation": "E2E 任务", "target": "工时", "fn": "sum"}]}, "PATCH")
    api(f"/api/dbs/{T}/schema", {"ops": [{"op": "add", "name": "编号", "type": "unique_id", "prefix": "T-"},
                                         {"op": "add", "name": "完成", "type": "button", "label": "标成完成", "set": {"状态": "已完成"}, "comment": "做完了，记一笔"},
                                         {"op": "add", "name": "附件", "type": "files"}]}, "PATCH")
    page.wait_for_selector(".db-th:has-text('附件')", timeout=5000)
    check(cell("写文案", "编号").inner_text().strip() == "T-1" and cell("做设计", "编号").inner_text().strip() == "T-2", "自动编号 T-1、T-2")
    cell("写文案", "完成").locator("button", has_text="标成完成").click()
    page.wait_for_selector(".dialog", timeout=5000)
    check("评论写好了" in page.locator(".dialog").inner_text(), "点按钮：提示评论写好了")
    page.locator(".dialog .modal-actions button").last.click()
    time.sleep(0.5)
    r = row(T, t1["id"])
    check(r["props"].get("status") == ST["已完成"], "点按钮把状态改成已完成")
    comments = api(f"/api/pages/{t1['id']}/comments")
    check(any(c["text"] == "做完了，记一笔" for c in comments), "按钮在这一行写了评论")
    f = Path(tempfile.gettempdir()) / "e2e-说明.txt"
    f.write_text("hello")
    cell("做设计", "附件").click()
    pop.locator("input[type=file]").set_input_files(str(f))
    page.wait_for_selector(".db-pop .db-file-row", timeout=5000)
    page.keyboard.press("Escape")
    time.sleep(0.5)
    files = row(T, t2["id"])["props"].get(schema(T)["附件"]["key"])
    check(files and files[0]["name"] == "e2e-说明.txt" and files[0]["url"].startswith("/api/media/"), f"传了一个文件：{files}")
    page.screenshot(path=str(HERE / "51-db-more-types.png"))

    # 项目库：反向关联和汇总
    page.goto(f"{BASE}/library/pages/{P}")
    page.wait_for_selector(".db-table", timeout=15000)
    check("写文案" in cell("官网改版", "E2E 任务").inner_text() and cell("官网改版", "总工时").inner_text().strip() == "4", "项目库：反向关联显示「写文案」，总工时 4")
    # 在反向那一半加上「做设计」：任务那边跟着变，汇总变成 9
    cell("官网改版", "E2E 任务").click()
    pop.locator(".db-picker-item", has_text="做设计").click()
    time.sleep(0.6)
    page.keyboard.press("Escape")
    page.wait_for_function("""[...document.querySelectorAll('.db-table tbody tr')].some(tr => tr.innerText.includes('官网改版') && tr.innerText.includes('9'))""", timeout=5000)
    check(row(T, t2["id"])["props"].get(schema(T)["项目"]["key"]) == [p1["id"]], "在反向那一半选的，改到了任务那边；总工时 9")
    page.screenshot(path=str(HERE / "52-db-mirror-rollup.png"))

    # 改视图记进历史，能撤销
    page.goto(f"{BASE}/library/pages/{T}")
    page.wait_for_selector(".db-table", timeout=15000)
    page.locator(".db-th", has_text="工时").first.click()
    pop.locator("button", has_text="降序").click()
    time.sleep(0.6)
    page.locator(".db-tool", has_text="历史").click()
    page.wait_for_selector(".db-history-item", timeout=5000)
    first = page.locator(".db-history-item").first
    check("改了视图「表格」" in first.inner_text(), "历史里有这次改视图：" + first.inner_text().replace("\n", " ")[:50])
    first.locator("button", has_text="撤销").click()
    page.locator(".dialog .modal-actions button").last.click()
    time.sleep(0.6)
    page.keyboard.press("Escape")
    check(not api(f"/api/dbs/{T}")["views"][0]["sort"], "撤销以后排序没了")

    # 几百行：先给 200 行，滚到底接着加载
    big = api("/api/dbs", {"title": "E2E 大库"})
    for k in range(450):
        api(f"/api/dbs/{big['id']}/rows", {"title": f"行 {k:03d}"})
    page.goto(f"{BASE}/library/pages/{big['id']}")
    page.wait_for_selector(".db-table tbody tr", timeout=15000)
    n0 = page.locator(".db-table tbody tr").count()
    check(n0 == 200 and "还有 250 行" in page.locator(".db-more").inner_text(), f"先显示 200 行，说还有 250 行（{n0}）")
    for _ in range(6):
        page.locator(".db-more").scroll_into_view_if_needed() if page.locator(".db-more").count() else None
        time.sleep(0.6)
    n1 = page.locator(".db-table tbody tr").count()
    check(n1 == 450 and page.locator(".db-more").count() == 0, f"滚到底加载完了 450 行（{n1}）")
    page.locator(".db-new").click()
    page.wait_for_selector(".db-td-title input", timeout=5000)
    page.keyboard.type("新的一行")
    page.keyboard.press("Enter")
    time.sleep(0.6)
    check(page.locator(".db-table tbody tr", has_text="新的一行").count() == 1, "大库里新建的行看得见")

    # 手机：长按卡片拖到“已完成”
    phone = browser.new_page(viewport={"width": 390, "height": 844}, is_mobile=True, has_touch=True)
    phone.on("pageerror", lambda e: errors.append(str(e)))
    phone.goto(f"{BASE}/library/pages/{T}")
    phone.wait_for_selector(".db-tab:has-text('看板')", timeout=15000)
    phone.locator(".db-tab", has_text="看板").tap()
    phone.wait_for_selector(".db-board", timeout=5000)
    api(f"/api/dbs/{T}/rows/{t2['id']}", {"props": {"status": ST["未开始"]}}, "PATCH")
    time.sleep(0.8)
    card = phone.locator(".db-card", has_text="做设计")
    card.scroll_into_view_if_needed()
    box = card.bounding_box()
    sx, sy = box["x"] + 30, box["y"] + 12
    phone.evaluate("""([x, y]) => {
        const el = document.elementFromPoint(x, y)
        const t = new Touch({ identifier: 1, target: el, clientX: x, clientY: y })
        el.dispatchEvent(new TouchEvent('touchstart', { touches: [t], targetTouches: [t], changedTouches: [t], bubbles: true, cancelable: true }))
        window.__dragEl = el
    }""", [sx, sy])
    time.sleep(0.5)
    # 往右拖：手指停在右边缘，看板自己往右滚，直到“已完成”那一列到手指底下
    for k in range(40):
        x, y = 360, sy + 40
        phone.evaluate("""([x, y]) => {
            const el = window.__dragEl
            const t = new Touch({ identifier: 1, target: el, clientX: x, clientY: y })
            el.dispatchEvent(new TouchEvent('touchmove', { touches: [t], targetTouches: [t], changedTouches: [t], bubbles: true, cancelable: true }))
        }""", [x, y])
        time.sleep(0.05)
        under = phone.evaluate("""(x) => [...document.querySelectorAll('.db-col')].find(c => { const r = c.getBoundingClientRect(); return x >= r.left && x <= r.right })
            ?.querySelector('.db-col-head')?.innerText || ''""", x)
        if "已完成" in under:
            break
    phone.screenshot(path=str(HERE / "53-db-touch-drag.png"))
    phone.evaluate("""([x, y]) => {
        const el = window.__dragEl
        const t = new Touch({ identifier: 1, target: el, clientX: x, clientY: y })
        el.dispatchEvent(new TouchEvent('touchend', { touches: [], targetTouches: [], changedTouches: [t], bubbles: true, cancelable: true }))
    }""", [360, sy + 40])
    time.sleep(0.8)
    check(row(T, t2["id"])["props"].get("status") == ST["已完成"], "手机上长按拖到「已完成」")
    check(phone.locator(".db-ghost").count() == 0 and phone.url.endswith(f"/library/pages/{T}"), "松手以后影子没了、也没打开那一行")
    check(not errors, f"页面上没有脚本错误：{errors[:3]}")
    browser.close()

for i in (T, P, big["id"]):
    api(f"/api/pages/{i}", method="DELETE")
print("失败", check.failed, "项")
