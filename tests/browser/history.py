"""版本历史：列出各版、和现在比（改过的段按字标出、删掉的段）、恢复一段（用回原来的编号）、只看这一版、回到这一版。"""
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


DOC = "# 方案\n\n第一段：先做调研。\n\n第二段：再写方案。\n\n第三段：最后评审。"
PID = api("/api/pages", {"bot_id": 1, "title": "方案", "content": DOC})["id"]
b = api(f"/api/pages/{PID}/blocks")
ids = b["order"]
api(f"/api/pages/{PID}/blocks", {"version": 1, "ops": [{"op": "replace", "block": ids[1], "markdown": "第一段：先做两周调研。"},
                                                       {"op": "delete", "block": ids[2]}]})
failed = 0


def check(ok, what):
    global failed
    print(("OK   " if ok else "FAIL ") + what)
    failed += 0 if ok else 1


def content():
    return api(f"/api/pages/{PID}")["content"]


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 1400, "height": 900})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(f"{BASE}/library/pages/{PID}")
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    page.locator(".doc-cbtn", has_text="历史").click()
    page.wait_for_selector(".dh-row")
    rows = page.locator(".dh-row").all_inner_texts()
    check(len(rows) == 2 and rows[0].startswith("你") and rows[1].startswith("Nova"), f"列出两版：{[r.splitlines()[0] for r in rows]}")
    page.locator(".dh-row").nth(1).click()
    page.wait_for_selector(".dhv-rows", timeout=8000)
    check(page.locator(".doc-wrap").is_hidden(), "看历史时编辑器藏起来了")
    check(page.locator(".dhv-row.change").count() == 1 and page.locator(".dhv-row.del").count() == 1, "和现在比：一段改了、一段删了")
    check(page.locator(".dhv-inline ins").inner_text() == "两周", "改过的段里标出了加的字：" + page.locator(".dhv-inline ins").inner_text())
    page.screenshot(path=str(HERE / "50-history.png"))
    page.locator(".dhv-row.del button", has_text="恢复成这一版的样子").click()
    page.wait_for_function("!document.querySelector('.dhv-row.del')", timeout=8000)
    after = api(f"/api/pages/{PID}/blocks")
    check(ids[2] in after["order"] and "第二段：再写方案。" in content(), "删掉的段恢复了，用回原来的编号")
    check(page.locator(".doc-body p", has_text="第二段").count() == 1, "编辑器里也回来了")
    page.locator(".dhv-head .panel-tabs button", has_text="只看这一版").click()
    check(page.locator(".doc-hview .md", has_text="第一段：先做调研。").count() >= 1, "只看这一版")
    page.locator(".dhv-head button", has_text="回到这一版").click()
    page.wait_for_selector(".dialog")
    page.locator(".dialog button", has_text="回到这一版").click()
    page.wait_for_selector(".doc-wrap:not([style*='none'])", timeout=8000)
    time.sleep(1)
    check(content() == DOC, "回到了第一版的内容")
    check(page.locator(".doc-body p", has_text="先做调研").count() == 1, "编辑器里是第一版的内容")
    vs = api(f"/api/pages/{PID}/versions")
    check(vs[0]["label"].startswith("restore:"), f"回到某一版也记成新的一版：{[v['label'] for v in vs]}")
    page.screenshot(path=str(HERE / "51-restored.png"))
    check(not errors, f"没有脚本错误：{errors[:3]}")
    browser.close()
sys.exit(1 if failed else 0)
