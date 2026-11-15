"""资料库块编辑器的浏览器检查（第二轮）：按需加载、地址和前进后退、锁定、分享页、断网、刷新不丢、手机和浅色主题。"""
import json
import os
import tempfile
import sys
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

BASE = os.environ.get("OSA_E2E_BASE", "http://127.0.0.1:8766")
HERE = Path(os.environ.get("OSA_E2E_OUT", tempfile.gettempdir()))   # 截图放这里
DOC = "# 第二轮\n\n> [!TIP]\n> 小提示\n\n<details><summary>展开</summary>\n\n里面的字\n\n</details>\n\n正文一段\n\n- 列表一\n- 列表二"
req = urllib.request.Request(f"{BASE}/api/pages", data=json.dumps({"bot_id": 2, "title": "第二轮", "content": DOC}).encode(),
                             headers={"content-type": "application/json"})
PID = json.load(urllib.request.urlopen(req))["id"]
failed = 0


def check(ok, what):
    global failed
    print(("OK   " if ok else "FAIL ") + what)
    failed += 0 if ok else 1


def content():
    return json.load(urllib.request.urlopen(f"{BASE}/api/pages/{PID}"))["content"]


def saved(pg, timeout=10000):
    pg.wait_for_function("document.querySelector('.doc-status')?.textContent.includes('已保存')", timeout=timeout)


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    ctx = browser.new_context(viewport={"width": 1280, "height": 900})
    page = ctx.new_page()
    errors, scripts, gets = [], [], []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.on("request", lambda r: (scripts.append(r.url) if r.url.endswith(".js") else None,
                                  gets.append(r.url) if r.method == "GET" and f"/api/pages/{PID}" in r.url else None))

    # 只开聊天：不下载编辑器
    page.goto(BASE + "/")
    page.wait_for_selector(".composer, .chat", timeout=15000)
    time.sleep(2)
    check(not any("DocEditor" in u for u in scripts), "只开聊天时没下载编辑器")

    # 资料库 → 页面：地址跟着变，刷新还在，后退回去
    page.locator(".side-nav", has_text="资料库").click()
    page.wait_for_selector(".lib-row")
    check(page.url.endswith("/library"), f"资料库的地址：{page.url}")
    page.locator(".lib-row", has_text="第二轮").first.click()
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    check(page.url.endswith(f"/library/pages/{PID}"), f"页面的地址：{page.url}")
    page.reload()
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    check(page.url.endswith(f"/library/pages/{PID}") and page.locator(".page-title").inner_text() == "第二轮", "刷新以后还在这一页")
    page.go_back()
    page.wait_for_selector(".lib-row", timeout=5000)
    check(page.url.endswith("/library"), "后退回到资料库列表")
    page.go_forward()
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    check(True, "前进回到页面")

    # 打字保存时，页面本身不会被整个重拉（只发 page_changed）
    gets.clear()
    page.locator(".doc-body p", has_text="正文一段").click()
    time.sleep(0.3)
    page.keyboard.press("End")
    page.keyboard.type("，加几个字")
    saved(page)
    time.sleep(1)
    check(not [u for u in gets if u.rstrip("/").endswith(f"/api/pages/{PID}")], f"保存时没有重拉整页：{gets}")
    check("正文一段，加几个字" in content(), "改动存上了")

    # 锁定：Bot 只能读，你照样能改
    page.locator(".page-more > .btn").click()
    page.locator(".page-more .menu button", has_text="锁定").click()
    page.wait_for_selector(".page-info:has-text('已锁定')", timeout=5000)
    page.locator(".doc-body p", has_text="正文一段").click()
    time.sleep(0.3)
    page.keyboard.press("End")
    page.keyboard.type("（锁定后）")
    saved(page)
    check("（锁定后）" in content(), "锁定后你照样能改")
    page.locator(".page-more > .btn").click()
    page.locator(".page-more .menu button", has_text="解锁").click()
    page.wait_for_selector(".page-info:not(:has-text('已锁定'))", timeout=5000)

    # 断网：显示离线，连上后自动存
    ctx.set_offline(True)
    try:
        page.locator(".doc-body li", has_text="列表二").click(timeout=5000)
    except Exception:
        page.screenshot(path=str(HERE / "dbg-offline.png"))
        print("modal:", page.evaluate("[...document.querySelectorAll('.modal, .dialog, [role=dialog]')].map(e => e.innerText.slice(0, 200))"))
        raise
    time.sleep(0.3)
    page.keyboard.press("End")
    page.keyboard.type("（断网时写的）")
    page.wait_for_function("document.querySelector('.doc-status').textContent.includes('离线')", timeout=10000)
    page.screenshot(path=str(HERE / "10-offline.png"))
    check(True, "断网时显示离线")
    ctx.set_offline(False)
    saved(page, 40000)
    check("- 列表二（断网时写的）" in content(), "连上以后自动存上了")

    # 打完字马上刷新：不丢（离开时 keepalive 发出去，或者下次打开时补存草稿）
    page.locator(".doc-body h1").click()
    time.sleep(0.3)
    page.keyboard.press("End")
    page.keyboard.type("！")
    page.reload()
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    time.sleep(1.5)
    check(content().startswith("# 第二轮！"), "打完字马上刷新也没丢")
    check(page.locator(".doc-body h1").inner_text() == "第二轮！", "刷新后编辑器里也是新的")

    # 分享页：提示框、折叠块照样显示，不加载编辑器
    share = json.load(urllib.request.urlopen(urllib.request.Request(f"{BASE}/api/pages/{PID}/share", data=b'{"on": true}',
                                                                     headers={"content-type": "application/json"})))
    sp = ctx.new_page()
    shared_scripts = []
    sp.on("request", lambda r: shared_scripts.append(r.url) if r.url.endswith(".js") else None)
    sp.goto(f"{BASE}/s/{share['share_token']}")
    sp.wait_for_selector(".shared .md", timeout=10000)
    check(sp.locator(".shared .callout.callout-tip").count() == 1, "分享页显示提示框")
    check(sp.locator(".shared details summary", has_text="展开").count() == 1, "分享页显示折叠块")
    check(not any("DocEditor" in u for u in shared_scripts), "分享页没加载编辑器")
    sp.screenshot(path=str(HERE / "11-shared.png"))

    # 浅色主题、手机宽度
    page.evaluate("document.documentElement.dataset.theme = 'light'")
    time.sleep(0.3)
    page.screenshot(path=str(HERE / "12-light.png"))
    mob = browser.new_context(viewport={"width": 390, "height": 844}, is_mobile=True, has_touch=True)
    mp = mob.new_page()
    mp.goto(f"{BASE}/library/pages/{PID}")
    mp.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    left = mp.evaluate("document.querySelector('.doc-body p').getBoundingClientRect().left")
    check(left >= 28, f"手机上正文离左边 {left:.0f}px（留出手势返回的边缘）")
    mp.screenshot(path=str(HERE / "13-mobile.png"))
    check(not errors, f"没有脚本错误：{errors[:5]}")
    browser.close()
print("page", PID)
sys.exit(1 if failed else 0)
