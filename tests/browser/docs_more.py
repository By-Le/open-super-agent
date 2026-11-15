"""资料库的几件小事：回收站（删了能放回去）、编辑器里传图片（按钮、粘贴）、评论框里打 @ 补全 Bot 的名字、
手机上段的菜单（上移、下移、删除）、含 HTML 的表格整组改了以后行的编号不变。不花钱（不调模型）。"""
import base64
import json
import os
import tempfile
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

BASE = os.environ.get("OSA_E2E_BASE", "http://127.0.0.1:8766")
HERE = Path(os.environ.get("OSA_E2E_OUT", tempfile.gettempdir()))   # 截图放这里
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAEklEQVR4nGP4z8DAwMDAwMAAABQAAf/pIXDXAAAAAElFTkSuQmCC")


def api(path: str, body=None, method=None):
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode() if body is not None else None, method=method,
                                 headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req))


def check(ok, what):
    print(("OK   " if ok else "FAIL ") + what)
    if not ok:
        check.failed += 1
check.failed = 0


def blocks(pid):
    return api(f"/api/pages/{pid}/blocks")["blocks"]


for old in api("/api/pages"):   # 上次没跑完留下的
    if old["title"] in ("E2E 小事", "E2E 表格", "E2E 段菜单"):
        api(f"/api/pages/{old['id']}", method="DELETE")
BOT = api("/api/bootstrap")["bots"][0]["name"]
DOC = api("/api/pages", {"title": "E2E 小事", "content": "# 小事\n\n第一段\n\n第二段\n\n第三段"})["id"]
TABLE = "| 名 | 值 |\n| --- | --- |\n| a | <b>1</b> |\n| b | 2 |"
TAB = api("/api/pages", {"title": "E2E 表格", "content": "# 表\n\n" + TABLE})["id"]
img = Path(tempfile.gettempdir()) / "e2e-小图.png"
img.write_bytes(PNG)

with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 1280, "height": 860})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.on("console", lambda m: m.type == "error" and "Failed to load resource" not in m.text and errors.append(m.text))

    # 传图片：斜杠菜单 → 图片 → 传图片
    page.goto(f"{BASE}/library/pages/{DOC}")
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    page.locator(".doc-body p", has_text="第三段").click()
    time.sleep(0.3)
    page.keyboard.press("End")
    page.keyboard.press("Enter")
    page.keyboard.type("/图片")
    page.wait_for_selector(".slash-menu", timeout=5000)
    page.keyboard.press("Enter")
    page.wait_for_selector(".doc-url-box", timeout=5000)
    with page.expect_file_chooser() as fc:
        page.locator(".doc-url-box button", has_text="传图片").click()
    fc.value.set_files(str(img))
    page.wait_for_selector(".doc-body img[src^='/api/media/']", timeout=8000)
    time.sleep(1.5)
    content = api(f"/api/pages/{DOC}")["content"]
    check("](/api/media/" in content and "e2e-小图" in content, "斜杠菜单传了一张图，存成 ![e2e-小图](/api/media/…)")
    # 粘贴图片
    n0 = page.locator(".doc-body img").count()
    page.locator(".doc-body p", has_text="第一段").click()
    page.evaluate("""(b64) => {
        const bin = Uint8Array.from(atob(b64), c => c.charCodeAt(0))
        const dt = new DataTransfer()
        dt.items.add(new File([bin], 'paste.png', { type: 'image/png' }))
        document.querySelector('.doc-body').dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }))
    }""", base64.b64encode(PNG).decode())
    page.wait_for_function(f"document.querySelectorAll('.doc-body img').length > {n0}", timeout=8000)
    check(True, "粘贴一张图也插进去了")
    page.screenshot(path=str(HERE / "60-doc-image.png"))

    # 评论框里打 @
    page.locator(".doc-cbtn", has_text="评论").click()
    page.locator(".dc-page", has_text="评论整页").click()
    page.wait_for_selector(".dc-composer textarea", timeout=5000)
    box = page.locator(".dc-composer textarea").first
    box.click()
    box.type("请 @" + BOT[:1])
    page.wait_for_selector(".dc-mentions button", timeout=3000)
    page.screenshot(path=str(HERE / "61-doc-mention.png"))
    page.keyboard.press("Enter")
    check(box.input_value() == f"请 @{BOT} ", f"@ 补全了 Bot 的名字：{box.input_value()!r}")
    page.keyboard.press("Escape")

    # 含 HTML 的表格：整组改一行，别的行编号不变
    page.goto(f"{BASE}/library/pages/{TAB}")
    page.wait_for_selector(".raw-block", timeout=15000)
    before = [b["id"] for b in blocks(TAB)]
    page.locator(".raw-block .raw-chip button", has_text="编辑").click()
    page.locator(".raw-edit").fill(TABLE.replace("| b | 2 |", "| b | 20 |") + "\n| c | 3 |")
    page.locator(".raw-block .raw-chip button", has_text="完成").click()
    time.sleep(2)
    after = blocks(TAB)
    ids = [b["id"] for b in after]
    check(ids[:4] == before and len(ids) == 5 and after[3]["md"] == "| b | 20 |", f"表格改了一行、加了一行：原来的编号都在（{before} → {ids}）")

    # 删除 → 回收站 → 放回去
    page.goto(f"{BASE}/library/pages/{TAB}")
    page.wait_for_selector(".page-more button", timeout=10000)
    page.on("dialog", lambda d: d.accept())
    page.locator(".page-more button").first.click()
    page.locator(".page-more .menu button", has_text="删除").click()
    page.wait_for_selector(".dialog", timeout=3000)
    check("回收站" in page.locator(".dialog").inner_text(), "删除时说会先放进回收站")
    page.locator(".dialog .modal-actions button").last.click()
    page.wait_for_selector(".lib-trash", timeout=8000)
    page.locator(".lib-trash").click()
    page.wait_for_selector(".trash-row", timeout=5000)
    row = page.locator(".trash-row", has_text="E2E 表格")
    check(row.count() >= 1 and "30 天后清掉" in row.first.inner_text(), "回收站里有它，写着 30 天后清掉")
    row = row.first
    page.screenshot(path=str(HERE / "62-trash.png"))
    row.locator("button", has_text="放回去").click()
    page.wait_for_selector(".dialog", timeout=5000)
    page.locator(".dialog .modal-actions button").last.click()
    back = next((p for p in api("/api/pages") if p["id"] == TAB), None)
    got = [b["id"] for b in blocks(back["id"])] if back else None
    check(back is not None and got == ids, f"放回去了，段的编号都没变（{got}）")

    # 手机：段的菜单
    phone = browser.new_page(viewport={"width": 390, "height": 844}, is_mobile=True, has_touch=True)
    phone.on("pageerror", lambda e: errors.append(str(e)))
    MENU = api("/api/pages", {"title": "E2E 段菜单", "content": "# 段菜单\n\n第一段\n\n第二段\n\n第三段"})["id"]
    order0 = [b["md"] for b in blocks(MENU)]
    phone.goto(f"{BASE}/library/pages/{MENU}")
    phone.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    phone.locator(".doc-body p", has_text="第一段").tap()
    phone.wait_for_selector(".doc-bh-btn", timeout=5000)
    phone.locator(".doc-bh-btn").tap()
    phone.screenshot(path=str(HERE / "63-block-menu.png"))
    phone.locator(".doc-bh-menu button", has_text="下移").tap()
    time.sleep(2)
    order1 = [b["md"] for b in blocks(MENU)]
    k = order0.index("第一段")
    check(order1.index("第一段") == k + 1 and order1[k] == order0[k + 1] and len(order1) == len(order0), "手机上把「第一段」往下挪了一段")
    phone.locator(".doc-body p", has_text="第二段").tap()
    phone.locator(".doc-bh-btn").tap()
    phone.locator(".doc-bh-menu button", has_text="删除这一段").tap()
    time.sleep(2)
    check("第二段" not in [b["md"] for b in blocks(MENU)], "手机上删了「第二段」")
    check(not errors, f"页面上没有脚本错误：{errors[:3]}")
    browser.close()

for p in api("/api/pages"):
    if p["title"] in ("E2E 小事", "E2E 表格", "E2E 段菜单"):
        api(f"/api/pages/{p['id']}", method="DELETE")
for x in api("/api/trash"):
    if x["title"] in ("E2E 小事", "E2E 表格", "E2E 段菜单"):
        api(f"/api/trash/{x['id']}", method="DELETE")
print("失败", check.failed, "项")
