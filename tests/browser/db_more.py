"""数据库的后续几项：行页面的“历史”里看属性改了什么、只恢复一个属性；表结构后来改过的旧值不比；
手机宽度下看板卡片的“⋯”换列、调顺序；设置里把撤销了的外部 Agent 从列表里删掉（名字还在）。不花钱（不调模型）。
行的改动要有两个人：同一个人 10 分钟内接着改会并成一版，所以“别人”是一个外部 Agent（经 /mcp 的 db_update）。"""
import asyncio
import concurrent.futures
import json
import os
import tempfile
import time
import urllib.request
from pathlib import Path

import httpx2
from mcp import Client
from mcp.client.streamable_http import streamable_http_client
from playwright.sync_api import sync_playwright

BASE = os.environ.get("OSA_E2E_BASE", "http://127.0.0.1:8766")
HERE = Path(os.environ.get("OSA_E2E_OUT", tempfile.gettempdir()))   # 截图放这里
NAME = f"E2E Agent {int(time.time()) % 10000}"


def api(path: str, body=None, method=None):
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode() if body is not None else None, method=method,
                                 headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req))


made = api("/api/agent-tokens", {"name": NAME, "scope": "write"})
TOKEN, AID = made["token"], made["id"]


def call(name: str, **args) -> str:
    """经真的 HTTP 调一次 MCP 工具，放在别的线程里跑（Playwright 的同步接口占着这个线程的事件循环）。"""
    async def go():
        hc = httpx2.AsyncClient(headers={"authorization": f"Bearer {TOKEN}"}, timeout=30)
        async with hc, Client(streamable_http_client(f"{BASE}/mcp", http_client=hc)) as c:
            r = await c.call_tool(name, args)
            return ("ERROR " if r.is_error else "") + "".join(getattr(x, "text", "") for x in r.content)
    with concurrent.futures.ThreadPoolExecutor(1) as pool:
        return pool.submit(asyncio.run, go()).result()


def check(ok, what):
    print(("OK   " if ok else "FAIL ") + what)
    if not ok:
        check.failed += 1
check.failed = 0


DB = api("/api/dbs", {"title": "E2E 数据库 2"})
ID = DB["id"]
ST = {o["name"]: o["id"] for o in DB["schema"][1]["options"]}
api(f"/api/dbs/{ID}/schema", {"ops": [{"op": "add", "name": "备注", "type": "text"}]}, "PATCH")
NOTE = next(p["key"] for p in api(f"/api/dbs/{ID}")["schema"] if p["name"] == "备注")
ROW = api(f"/api/dbs/{ID}/rows", {"title": "写周报", "props": {"status": ST["进行中"], NOTE: "abc"}})
RID = ROW["id"]
out = call("db_update", rows=[{"row_id": RID, "props": {"状态": "已完成", "名称": "写周报（终稿）"}, "version": ROW["version"]}])
print("db", ID, "row", RID, "|", out.splitlines()[0])


def row():
    return api(f"/api/dbs/{ID}/rows/{RID}")["row"]


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 1280, "height": 860})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.on("console", lambda m: m.type == "error" and errors.append(m.text))

    # 行页面的历史：两版（用户建的、Agent 改的）
    page.goto(f"{BASE}/library/pages/{RID}")
    page.wait_for_selector(".db-props", timeout=15000)
    page.wait_for_selector(".doc-cbtn:has-text('历史')", timeout=15000)
    page.locator(".doc-cbtn", has_text="历史").click()
    page.wait_for_selector(".dh-row", timeout=5000)
    check(page.locator(".dh-row").count() == 2 and NAME in page.locator(".dh-row").first.inner_text(), "行的历史有两版，最新的是外部 Agent 改的")

    # 最新那版“这一版改了什么”：状态、名称
    page.locator(".dh-row").first.click()
    page.wait_for_selector(".doc-hview", timeout=5000)
    page.locator(".dhv-head .panel-tabs button", has_text="这一版改了什么").click()
    page.wait_for_selector(".dhv-prop", timeout=5000)
    props = page.locator(".dhv-props").inner_text()
    check("状态" in props and "进行中" in props and "已完成" in props and "写周报（终稿）" in props, "这一版改了什么：状态 进行中 → 已完成、名称也改了：" + props.replace("\n", " ")[:90])
    check("正文没变" in page.locator(".doc-hview").inner_text(), "只改了属性时说正文没变")
    page.screenshot(path=str(HERE / "45-row-history-prev.png"))

    # 第一版“和现在比”：只把状态恢复回去
    page.locator(".dh-row").nth(1).click()
    page.wait_for_selector(".dhv-prop:has-text('状态')", timeout=5000)
    st = page.locator(".dhv-prop", has_text="状态")
    check("后来改了" in st.inner_text(), "和现在比：状态后来改了")
    st.locator("button", has_text="恢复成这一版的样子").click()
    time.sleep(0.8)
    check(row()["props"].get("status") == ST["进行中"] and row()["title"] == "写周报（终稿）", "只恢复了状态，名称没动")
    try:
        page.wait_for_selector(".db-prop:has-text('状态') .db-chip:has-text('进行中')", state="attached", timeout=5000)
        check(True, "属性栏跟着变回进行中")
    except Exception as e:
        check(False, "属性栏没跟着变：" + str(e).splitlines()[0])
    page.wait_for_function("document.querySelectorAll('.dhv-prop').length === 1", timeout=5000)
    check("名称" in page.locator(".dhv-props").inner_text(), "恢复以后对比里只剩名称不同")
    page.wait_for_function("document.querySelectorAll('.dh-row').length === 3", timeout=5000)
    check(True, "恢复记成了新的一版")

    # 只看这一版：列出那一版的属性
    page.locator(".dhv-head .panel-tabs button", has_text="只看这一版").click()
    snap = page.locator(".dhv-props").inner_text()
    check("进行中" in snap and "abc" in snap, "只看这一版：列出那一版的状态和备注：" + snap.replace("\n", " ")[:60])

    # 备注改成数字：旧值 "abc" 认不出，对比里不比、提一句，回到这一版也不带回来
    api(f"/api/dbs/{ID}/schema", {"ops": [{"op": "update", "prop": "备注", "type": "number"}]}, "PATCH")
    page.locator(".dh-row").last.click()
    page.locator(".dhv-head .panel-tabs button", has_text="和现在比").click()
    page.wait_for_selector(".dhv-props:has-text('表结构后来改过')", timeout=5000)
    check("备注" in page.locator(".dhv-props").inner_text() and page.locator(".dhv-prop", has_text="备注").count() == 0, "改了类型的备注不比，只提一句")
    page.screenshot(path=str(HERE / "46-row-history-stale.png"))
    page.on("dialog", lambda d: d.accept())
    page.locator(".dhv-head .btn", has_text="回到这一版").click()
    page.locator(".dialog .modal-actions button").last.click()
    time.sleep(0.8)
    r = row()
    check(r["title"] == "写周报" and r["props"].get("status") == ST["进行中"] and NOTE not in r["props"], f"回到第一版：名称回去了，备注没变成 abc：{r['title']} {r['props']}")

    # 手机：看板卡片的“⋯”换列、调顺序
    second = api(f"/api/dbs/{ID}/rows", {"title": "第二张", "props": {"status": ST["已完成"]}})
    phone = browser.new_page(viewport={"width": 390, "height": 844}, is_mobile=True, has_touch=True)
    phone.on("pageerror", lambda e: errors.append(str(e)))
    phone.goto(f"{BASE}/library/pages/{ID}")
    phone.wait_for_selector(".db-tab:has-text('看板')", timeout=15000)
    phone.locator(".db-tab", has_text="看板").tap()
    phone.wait_for_selector(".db-board", timeout=5000)
    phone.locator(".db-card", has_text="写周报").locator(".db-card-more").tap()
    phone.wait_for_selector(".db-pop.sheet .db-card-menu", timeout=5000)
    phone.screenshot(path=str(HERE / "47-board-menu-phone.png"))
    phone.locator(".db-pop .db-card-to", has_text="已完成").tap()
    time.sleep(0.8)
    check(row()["props"].get("status") == ST["已完成"], "手机上卡片菜单把“写周报”挪到已完成")
    done = phone.locator(".db-col", has=phone.locator(".db-col-head .db-chip", has_text="已完成"))
    titles = lambda: [x.inner_text() for x in done.locator(".db-card-title").all()]
    check(titles() == ["第二张", "写周报"], f"挪过去排在那一列最后：{titles()}")
    done.locator(".db-card", has_text="写周报").locator(".db-card-more").tap()
    phone.locator(".db-pop button", has_text="往上挪").tap()
    time.sleep(0.8)
    check(titles() == ["写周报", "第二张"], f"往上挪了一格：{titles()}")
    done.locator(".db-card", has_text="写周报").locator(".db-card-more").tap()
    check(phone.locator(".db-pop button", has_text="往上挪").is_disabled(), "最上面那张不能再往上挪")
    phone.keyboard.press("Escape")
    check(phone.evaluate("document.documentElement.scrollWidth <= innerWidth + 1"), "手机宽度下页面不横向滚动")

    # 设置：撤销了的收起来，能从列表里删掉，名字还在
    api(f"/api/agent-tokens/{AID}", method="DELETE")
    page.evaluate("window.dispatchEvent(new CustomEvent('osa-open-settings', { detail: 'agents' }))")
    page.wait_for_selector(".agent-gone summary", timeout=10000)
    check(page.locator(f".agent-row.revoked:has-text('{NAME}')").is_hidden(), "撤销了的默认收起来")
    page.locator(".agent-gone summary").click()
    gone = page.locator(".agent-row", has_text=NAME)
    gone.locator("button", has_text="从列表里删掉").click()
    page.wait_for_selector(f".agent-row:has-text('{NAME}')", state="detached", timeout=5000)
    check(AID not in [x["id"] for x in api("/api/agent-tokens")], "从列表里删掉了")
    check({"id": AID, "name": NAME, "revoked": True} in api("/api/bootstrap")["agents"], "名字还在（它改过的那版历史照样署它的名字）")
    check(not errors, f"页面上没有脚本错误：{errors[:3]}")
    browser.close()

api(f"/api/pages/{ID}", method="DELETE")
print("失败", check.failed, "项")
