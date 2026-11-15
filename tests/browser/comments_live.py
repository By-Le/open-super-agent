"""评论（真模型）：选中文字 → 评论 → 叫醒写这段的 Nova → Nova 改页面、在评论里回复；面板、段旁边的个数、“正在处理”。"""
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
DOC = "# 午饭清单\n\n今天中午大家想吃点什么呢，我想了想，可能吃面条比较好，因为面条比较快，而且也比较便宜，大家觉得怎么样呢。\n\n- 面条\n- 米饭"
req = urllib.request.Request(f"{BASE}/api/pages", data=json.dumps({"bot_id": 1, "title": "午饭", "content": DOC}).encode(),
                             headers={"content-type": "application/json"})
PID = json.load(urllib.request.urlopen(req))["id"]
failed = 0


def check(ok, what):
    global failed
    print(("OK   " if ok else "FAIL ") + what)
    failed += 0 if ok else 1


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 1400, "height": 900})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(f"{BASE}/library/pages/{PID}")
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    para = page.locator(".doc-body p", has_text="今天中午")
    para.click(click_count=3)   # 选中整段
    time.sleep(0.4)
    page.wait_for_selector(".doc-toolbar", timeout=5000)
    page.locator(".doc-toolbar button[title='评论']").click()
    page.wait_for_selector(".doc-comments .dc-new textarea", timeout=5000)
    check("Nova" in page.locator(".dc-new select").inner_text(), "默认叫写这段的 Nova")
    page.locator(".dc-new textarea").fill("这段太啰嗦了，改成一句话，保留“面条快、便宜”的意思")
    page.locator(".dc-new button", has_text="发送").click()
    page.wait_for_selector(".dc-pending", timeout=8000)
    check(True, "评论发出去了，显示“正在处理”：" + page.locator(".dc-pending").inner_text())
    check(page.locator(".doc-body .blk-c").count() == 1, "那一段旁边显示评论个数")
    page.screenshot(path=str(HERE / "30-comment-pending.png"))
    try:
        page.wait_for_selector(".dc-thread .dc-meta b:text-is('Nova')", timeout=180000)
        time.sleep(1.5)
        items = page.locator(".dc-thread .dc-item").all_inner_texts()
        check(any(t.startswith("Nova") for t in items), f"Nova 在评论里回复了：{[t[:60] for t in items]}")
    except Exception as e:
        check(False, "没等到 Nova 的回复：" + str(e).splitlines()[0])
    time.sleep(2)
    content = json.load(urllib.request.urlopen(f"{BASE}/api/pages/{PID}"))["content"]
    new_para = content.split("\n\n")[1]
    check("今天中午大家想吃点什么呢" not in content and "面条" in new_para, f"Nova 改了那一段：{new_para[:80]}")
    blocks = json.load(urllib.request.urlopen(f"{BASE}/api/pages/{PID}/blocks"))["blocks"]
    check(blocks[1]["by"] == "bot:1" and blocks[0]["v"] == 1 and blocks[2]["v"] == 1, "只有那一段变了，作者是 Nova")
    check(page.locator(".doc-body p", has_text=new_para[:6]).count() == 1, "编辑器里实时变成了新的内容")
    page.screenshot(path=str(HERE / "31-comment-done.png"))
    comments = json.load(urllib.request.urlopen(f"{BASE}/api/pages/{PID}/comments"))
    print("评论：", [(c["by"], c["text"][:50], c["resolved"]) for c in comments])
    check(not errors, f"没有脚本错误：{errors[:3]}")
    browser.close()
print("page", PID)
sys.exit(1 if failed else 0)
