"""页面层级：列表里新建子页面、页面底下的“＋ 子页面”、树展开收起（记得住）、页面路径和返回上一级、子页面列表、挪到最上层、删父页面时子页面上移。"""
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
for _ in range(60):
    try:
        urllib.request.urlopen(BASE + "/api/pages")
        break
    except Exception:
        time.sleep(0.5)


def api(path, body=None, method=None):
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode() if body is not None else None,
                                 headers={"content-type": "application/json"}, method=method or ("POST" if body is not None else "GET"))
    return json.load(urllib.request.urlopen(req))


TAG = str(int(time.time()))[-5:]
parent = api("/api/pages", {"bot_id": 1, "title": f"项目{TAG}", "content": "# 项目总览\n\n总览内容"})["id"]
failed = 0


def check(ok, what):
    global failed
    print(("OK   " if ok else "FAIL ") + what)
    failed += 0 if ok else 1


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 1280, "height": 900})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(f"{BASE}/library")
    page.wait_for_selector(".tree-row")
    row = page.locator(".tree-row", has_text=f"项目{TAG}")
    row.hover()
    row.locator("button[title='新建子页面']").click()
    page.wait_for_selector(".doc-body", timeout=15000)
    child = int(page.url.rsplit("/", 1)[1])
    check(api(f"/api/pages/{child}")["parent_id"] == parent, "列表里点“＋”建的是子页面")
    check(page.locator(".page-head .back").inner_text().endswith(f"项目{TAG}"), "返回按钮回到上一级：" + page.locator(".page-head .back").inner_text())
    check(f"项目{TAG}" in page.locator(".page-path").inner_text(), "页面顶上有路径：" + page.locator(".page-path").inner_text())
    page.keyboard.type("子页面里写的字")   # 新页面直接能打字
    page.wait_for_function("document.querySelector('.doc-status').textContent.includes('已保存')", timeout=8000)
    # 还没有子页面的页面：底下的“＋ 子页面”要点得动（它往上收进了编辑器的下边距，曾经被编辑器盖住、点了没反应）
    try:
        page.locator(".page-kids button", has_text="子页面").click(timeout=5000)
        page.wait_for_url(lambda u: not u.endswith(f"/{child}"), timeout=8000)
        grand = int(page.url.rsplit("/", 1)[1])
        check(api(f"/api/pages/{grand}")["parent_id"] == child, "页面底下的“＋ 子页面”点得动，建在这一页下面")
        api(f"/api/pages/{grand}", method="DELETE")
    except Exception as e:
        check(False, "页面底下的“＋ 子页面”点不动：" + str(e).splitlines()[0])
    page.goto(f"{BASE}/library/pages/{child}")
    page.wait_for_selector(".doc-body")
    page.locator(".page-head .back").click()
    page.wait_for_selector(".page-kid", timeout=8000)
    check(page.locator(".page-kid").inner_text().endswith("无标题"), "父页面底下列出了子页面")
    page.locator(".page-path .link-btn, .page-head .back").first.click()   # 回到资料库列表
    page.wait_for_selector(".tree-row")
    prow = page.locator(".tree-row", has_text=f"项目{TAG}")
    check(prow.locator(".tree-chev").inner_text() == "▸", "有子页面的显示展开箭头")
    prow.locator(".tree-chev").click()
    page.wait_for_selector(f".tree-row:has-text('无标题')")
    page.reload()
    page.wait_for_selector(".tree-row")
    check(page.locator(".tree-row", has_text="无标题").count() >= 1, "刷新后展开状态还在")
    page.screenshot(path=str(HERE / "40-tree.png"))
    # 挪到最上层
    crow = page.locator(".tree-row", has_text="无标题").first
    crow.hover()
    crow.locator(".page-more > .icon-btn").click()
    page.locator(".tree-row .menu button", has_text="挪到…").click()
    page.wait_for_selector(".move-dialog")
    titles = page.locator(".move-list button").all_inner_texts()
    check(not any(t.startswith("无标题") and "现在在这里" not in t for t in titles if t == "无标题"), "挪的对话框里没有它自己")
    page.locator(".move-list button", has_text="最上层").click()
    time.sleep(1)
    check(api(f"/api/pages/{child}")["parent_id"] is None, "挪到了最上层")
    # 再挪回去，然后删父页面：子页面挪到上一级（最上层）
    api(f"/api/pages/{child}/move", {"parent_id": parent})
    page.goto(f"{BASE}/library/pages/{parent}")
    page.wait_for_selector(".doc-body")
    page.locator(".page-head .page-more > .btn").click()
    page.locator(".page-head .menu button", has_text="删除").click()
    page.wait_for_selector(".dialog")
    check("1 个子页面会挪到上一级" in page.locator(".dialog").inner_text(), "删除时说明子页面会上移")
    page.locator(".dialog button", has_text="删除").click()
    time.sleep(1)
    check(api(f"/api/pages/{child}")["parent_id"] is None, "删了父页面，子页面挪到了最上层")
    check(not errors, f"没有脚本错误：{errors[:3]}")
    browser.close()
    api(f"/api/pages/{child}", method="DELETE")
sys.exit(1 if failed else 0)
