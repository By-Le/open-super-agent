"""外部 Agent（osa/agentapi.py）：用 MCP 客户端经 /mcp 建页面、按段改、写评论，网页上开着这个页面，看实时合进来、
“正在编辑”和作者条显示 token 的名字；页面列表写着是它建的；设置里能看到、能撤销，撤销后再调被拒。不花钱（不调模型）。"""
import asyncio
import concurrent.futures
import json
import os
import re
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
NAME = f"Claude Code E2E {int(time.time()) % 10000}"


def api(path: str, body=None, method=None):
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode() if body is not None else None, method=method,
                                 headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req))


made = api("/api/agent-tokens", {"name": NAME, "scope": "write"})
TOKEN, AID = made["token"], made["id"]


def call(name: str, **args) -> str:
    """经真的 HTTP 调一次工具（每次一个新连接：服务是无状态的）。放在别的线程里跑：Playwright 的同步接口占着这个线程的事件循环。"""
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


out = call("create_page", title="外部 Agent 写的页面", content="# 调研\n\n正文一段\n\n- 列表一\n- 列表二")
PID = int(re.search(r"#(\d+)「", out)[1])
print("page", PID, "token", AID)
with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 1280, "height": 900})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.on("console", lambda m: m.type == "error" and errors.append(m.text))
    page.goto(f"{BASE}/library/pages/{PID}")
    page.wait_for_selector(".doc-body [data-bid]", timeout=15000)
    time.sleep(0.5)
    first = page.locator(".doc-body h1")
    check(first.get_attribute("data-by") == NAME, f"作者条显示 token 的名字：{first.get_attribute('data-by')!r}")

    text = call("read_page", page_id=PID)
    version = int(re.search(r"版本 (\d+)", text)[1])
    ids = re.findall(r"^⟦(\w{4})⟧$", text, re.M)   # 单独一行的才是段落编号（说明文字里也有个 ⟦xxxx⟧）
    out = call("edit_page", page_id=PID, version=version, ops=[
        {"op": "replace", "block": ids[1], "markdown": "这段是外部 Agent 用 edit_page 改的。"},
        {"op": "append", "markdown": "- 列表三"}])
    check(out.startswith("已改页面"), "edit_page：" + out[:60])
    try:
        page.wait_for_selector(".doc-editing", timeout=5000)
        check(NAME in page.locator(".doc-editing").inner_text(), "“正在编辑”：" + page.locator(".doc-editing").inner_text())
        page.wait_for_selector(".doc-body p:has-text('这段是外部 Agent 用 edit_page 改的')", timeout=5000)
        check(True, "改动实时合进了打开着的编辑器")
        check(page.locator(".doc-body ul:has(li:has-text('列表二')) li:has-text('列表三')").count() == 1,
              "加的列表项接在原来的列表里（没被末尾的空段隔开）")
    except Exception as e:
        check(False, "没等到改动：" + str(e).splitlines()[0])
    p = page.locator(".doc-body p", has_text="这段是外部 Agent")
    check(p.get_attribute("data-by") == NAME, f"改过的段作者条是它：{p.get_attribute('data-by')!r}")
    time.sleep(0.3)
    page.screenshot(path=str(HERE / "30-agent-edit.png"))

    out = call("comment_page", page_id=PID, block=ids[1], text="这段我按最新数据改过了，请看一下")
    check("写了评论" in out, "comment_page：" + out[:60])
    try:
        page.wait_for_selector(f".doc-body [data-bid='{ids[1]}'][data-comments]", timeout=5000)
        check(True, "评论数实时出现在那一段上")
    except Exception as e:
        check(False, "评论数没出现：" + str(e).splitlines()[0])
    comments = api(f"/api/pages/{PID}/comments")
    check(len(comments) == 1 and comments[0]["by"] == f"agent:{AID}" and not comments[0]["woke"], "评论记成 agent、没叫醒 Bot")

    page.goto(f"{BASE}/library")
    page.wait_for_selector(f".lib-meta:has-text('{NAME}')", timeout=10000)
    check(True, "页面列表写着是它建的")
    page.evaluate("window.dispatchEvent(new CustomEvent('osa-open-settings', { detail: 'agents' }))")
    page.wait_for_selector(f".agent-row:has-text('{NAME}')", timeout=10000)
    row = page.locator(".agent-row", has_text=NAME)
    check("最后用过" in row.inner_text(), "设置里列着它和最后用过的时间：" + row.inner_text().replace("\n", " "))
    page.screenshot(path=str(HERE / "31-agent-settings.png"))
    page.on("dialog", lambda d: d.accept())
    row.locator("button", has_text="撤销").click()
    try:
        page.locator(".dialog .btn.danger, .dialog button:has-text('撤销')").last.click(timeout=3000)
    except Exception:
        pass
    page.wait_for_selector(f".agent-row.revoked:has-text('{NAME}')", timeout=5000)
    check(True, "撤销以后列表里标成已撤销")
    page.screenshot(path=str(HERE / "32-agent-revoked.png"))
    try:
        call("read_page", page_id=PID)
        check(False, "撤销以后还能调")
    except BaseException:   # SDK 客户端只说“Server returned an error response”，状态码直接发请求看
        r = httpx2.post(f"{BASE}/mcp", headers={"authorization": f"Bearer {TOKEN}", "accept": "application/json, text/event-stream"},
                        json={"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}})
        check(r.status_code == 401, f"撤销以后再调被拒：{r.status_code} {r.text[:60]}")
    check(not errors, f"页面上没有脚本错误：{errors[:3]}")
    browser.close()

api(f"/api/pages/{PID}", method="DELETE")
print("失败", check.failed, "项")
