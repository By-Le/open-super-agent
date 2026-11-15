"""真模型：Nova 在对话里用 read_page + edit_page 改页面，网页上开着这个页面，看实时合进来、作者条、“正在编辑”。"""
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
_req = urllib.request.Request(f"{BASE}/api/pages", data=json.dumps({"bot_id": 2, "title": "Bot 改页面", "content": "# 测试\n\n正文一段\n\n- 列表一"}).encode(),
                              headers={"content-type": "application/json"})
PID = json.load(urllib.request.urlopen(_req))["id"]


def blocks():
    return json.load(urllib.request.urlopen(f"{BASE}/api/pages/{PID}/blocks"))


before = {b["id"]: b for b in blocks()["blocks"]}
with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 1280, "height": 900})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(f"{BASE}/library/pages/{PID}")
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    msg = (f"请用 read_page 读一下 /library/pages/{PID}，然后用 edit_page 只把以「正文一段」开头的那一段换成"
           f"「这段是 Nova 用 edit_page 改的。」，别的段不要动。改完一句话告诉我就行。")
    page.evaluate("""([text]) => new Promise(ok => { const ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws');
      ws.onopen = () => { ws.send(JSON.stringify({ type: 'send', chat_id: 1, text })); setTimeout(() => { ws.close(); ok(true) }, 500) } })""", [msg])
    t0 = time.time()
    try:
        page.wait_for_selector(".doc-editing", timeout=180000)
        print(f"OK   {time.time() - t0:.0f} 秒后出现：", page.locator(".doc-editing").inner_text())
        page.wait_for_selector(".doc-body p:has-text('这段是 Nova 用 edit_page 改的')", timeout=10000)
        print("OK   改动实时合进了打开着的编辑器")
        time.sleep(0.3)
        page.screenshot(path=str(HERE / "20-bot-edit.png"))
        p = page.locator(".doc-body p", has_text="这段是 Nova")
        print("OK   作者条：", p.get_attribute("data-by"), p.get_attribute("class"))
    except Exception as e:
        print("FAIL 没等到 Bot 的改动：", str(e).splitlines()[0])
        page.screenshot(path=str(HERE / "20-bot-edit-fail.png"))
    after = {b["id"]: b for b in blocks()["blocks"]}
    changed = [(i, after[i]["md"], after[i]["by"]) for i in after if i not in before or before[i]["md"] != after[i]["md"]]
    gone = [i for i in before if i not in after]
    print("服务器上改动的段：", changed, "删掉的：", gone)
    print("脚本错误：", errors[:3])
    browser.close()
