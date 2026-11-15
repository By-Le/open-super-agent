"""资料库块编辑器的浏览器检查（第一轮）：打开不保存、改一个词只发一段、回车加列表项、斜杠菜单、两个窗口合并与冲突。"""
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
DOC = (Path(__file__).parents[1] / "fixtures" / "md_blocks" / "report.md").read_text() + \
    "\n\n<div>html 段</div>\n\n[ref]: https://example.com\n\n| a<br>b | c |\n| - | - |\n| 1 | 2 |\n"
_req = urllib.request.Request(f"{BASE}/api/pages", data=json.dumps({"bot_id": 1, "title": "E2E 周报", "content": DOC}).encode(),
                              headers={"content-type": "application/json"})
PID = json.load(urllib.request.urlopen(_req))["id"]
print("page", PID)


def blocks():
    return json.load(urllib.request.urlopen(f"{BASE}/api/pages/{PID}/blocks"))


def check(ok, what):
    print(("OK   " if ok else "FAIL ") + what)
    if not ok:
        check.failed += 1
check.failed = 0


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    ctx = browser.new_context(viewport={"width": 1280, "height": 900})
    page = ctx.new_page()
    errors, posts = [], []
    page.on("console", lambda m: m.type == "error" and errors.append(m.text))
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.on("request", lambda r: r.method == "POST" and "/blocks" in r.url and posts.append(json.loads(r.post_data or "{}")))
    before = blocks()
    page.goto(f"{BASE}/library/pages/{PID}")
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    time.sleep(3)
    page.screenshot(path=str(HERE / "01-open.png"), full_page=True)
    check(not posts, f"打开页面 3 秒内没有保存请求（{len(posts)}）")
    raws = page.locator(".raw-block").count()
    check(raws == 2, f"认不全的段（HTML、引用式链接定义）显示成原文块：{raws} 个")
    check(page.locator(".doc-body table").count() == 2, "两张表都是可编辑的表格（格子里有 <br> 的那张也是）")
    check(page.locator(".doc-body [data-callout]").count() == 1, "提示框")
    check(page.locator(".doc-body .doc-details").count() == 1, "折叠块")
    check(page.locator(".doc-body .blk-bot").count() > 10, "Bot 写的段画着作者条")
    check(page.url.endswith(f"/library/pages/{PID}"), "地址是 /library/pages/<id>")

    # 改一个词：只发这一段
    para = page.locator(".doc-body p", has_text="本周看了 5 家")
    box = para.bounding_box()
    para.click(position={"x": box["width"] - 4, "y": box["height"] - 6})   # 点在最后一行的末尾（软换行现在是真的换行，End 只到这一行末尾）
    time.sleep(0.3)
    page.keyboard.press("End")
    page.keyboard.type("（已核实）")
    page.wait_for_function("document.querySelector('.doc-status').textContent.includes('已保存')", timeout=8000)
    time.sleep(0.5)
    ops = [o for p in posts for o in p["ops"]]
    check(len(posts) == 1 and len(ops) == 1 and ops[0]["op"] == "replace", f"改一个词只发一个 replace：{ops}")
    after = blocks()
    old = {b["id"]: b["md"] for b in before["blocks"]}
    new = {b["id"]: b["md"] for b in after["blocks"]}
    changed = [i for i in old if old[i] != new.get(i)]
    check(len(changed) == 1 and new[changed[0]] == "本周看了 5 家，**两家**已经上线类似功能。\n详细对比见下表。（已核实）",
          f"服务器上只有那一段变了，原来的软换行还是软换行：{[new.get(i) for i in changed]}")
    check(after["order"] == before["order"], "段的顺序没变")

    # 列表项末尾回车加一项
    posts.clear()
    item = page.get_by_text("补 C 的截图", exact=True)
    item.click()
    time.sleep(0.3)   # 点击后的选区是异步交给编辑器的，马上按键会用旧的选区
    page.keyboard.press("End")
    page.keyboard.press("Enter")
    page.keyboard.type("补 D 的报价")
    page.wait_for_function("document.querySelector('.doc-status').textContent.includes('已保存')", timeout=8000)
    time.sleep(0.5)
    ops = [o for p in posts for o in p["ops"]]
    check([o["op"] for o in ops] == ["insert"] and ops[0]["markdown"] == "- [ ] 补 D 的报价", f"回车加的待办是一个 insert：{ops}")
    md = urllib.request.urlopen(f"{BASE}/api/pages/{PID}").read().decode()
    check("- [ ] 补 C 的截图\\n- [ ] 补 D 的报价" in md, "新待办紧挨着上一项（同一个列表）")

    # 斜杠菜单：插提示框
    posts.clear()
    last = page.locator(".doc-body p", has_text="最后更新")
    last.click()
    time.sleep(0.3)
    page.keyboard.press("End")
    page.keyboard.press("Enter")
    page.keyboard.type("/")
    page.wait_for_selector(".slash-menu", timeout=3000)
    page.screenshot(path=str(HERE / "02-slash.png"))
    page.keyboard.type("tsk")
    page.keyboard.press("Enter")
    page.keyboard.type("提示框里的字")
    page.wait_for_function("document.querySelector('.doc-status').textContent.includes('已保存')", timeout=8000)
    time.sleep(0.5)
    md = json.loads(urllib.request.urlopen(f"{BASE}/api/pages/{PID}").read())["content"]
    check("> [!NOTE]\n> 提示框里的字" in md, "斜杠菜单插的提示框存成 > [!NOTE]")

    # 两个窗口：改不同的段互相合进来；改同一段出冲突提示
    page2 = ctx.new_page()
    page2.on("pageerror", lambda e: errors.append("p2: " + str(e)))
    page2.goto(f"{BASE}/library/pages/{PID}")
    page2.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    h = page2.locator(".doc-body h2", has_text="结论")
    h.click()
    time.sleep(0.3)
    page2.keyboard.press("End")
    page2.keyboard.type("（二号窗口）")
    page2.wait_for_function("document.querySelector('.doc-status').textContent.includes('已保存')", timeout=8000)
    page.wait_for_selector(".doc-body h2:has-text('（二号窗口）')", timeout=8000)
    check(True, "一号窗口收到了二号窗口的改动")
    # 同一段：两边都改，晚存的那边看到冲突
    for pg in (page, page2):
        pg.locator(".doc-body h2", has_text="待办").click()
        time.sleep(0.3)
        pg.keyboard.press("End")
    page.keyboard.type("A")
    page2.keyboard.type("B")
    page2.wait_for_selector(".doc-banner, .doc-status:has-text('已保存')", timeout=10000)
    time.sleep(2.5)
    banners = page.locator(".doc-banner").count() + page2.locator(".doc-banner").count()
    page.screenshot(path=str(HERE / "03-conflict-1.png"))
    page2.screenshot(path=str(HERE / "03-conflict-2.png"))
    check(banners == 1, f"同一段两边都改：有一边出冲突提示（{banners}）")
    if banners:
        side = page if page.locator(".doc-banner").count() else page2
        side.locator(".doc-banner button", has_text="用 TA 的").click()
        time.sleep(1.5)
        a = page.locator(".doc-body h2", has_text="待办").inner_text()
        b = page2.locator(".doc-body h2", has_text="待办").inner_text()
        check(a == b, f"选“用 TA 的”之后两边一样：{a!r} / {b!r}")
    check(not errors, f"没有脚本错误：{errors[:5]}")
    browser.close()
sys.exit(1 if check.failed else 0)
