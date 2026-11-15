"""真模型端到端：群里 Researcher 写一页调研、交接给 Nova，Nova 在同一个页面上接着改（不用 Notion）。
看：页面只有一个；两个 Bot 的段作者分开；版本历史里两个人的修改都在；Nova 用的是 edit_page / 带 version 的 save_page。"""
import asyncio
import json
import os
import tempfile
import time
import urllib.request

import websockets

BASE = os.environ.get("OSA_E2E_BASE", "http://127.0.0.1:8766")


def api(path, body=None, method=None):
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode() if body is not None else None,
                                 headers={"content-type": "application/json"}, method=method or ("POST" if body is not None else "GET"))
    return json.load(urllib.request.urlopen(req))


group = api("/api/groups", {"bot_ids": [3, 1], "title": "交接测试", "lead": 3})
gid = group["id"] if "id" in group else group["chat_id"]
print("group", gid)
before = {p["id"] for p in api("/api/pages")}
msg = ("@Researcher 先写一页很短的《咖啡机选购要点》页面（三个小标题、每个小标题下一两句话，不用查资料，凭常识写，用 save_page 存成页面），"
       "存好后用 handoff 交给 @Nova。@Nova 接手后用 read_page 读这个页面，再用 edit_page 在最后加一个「预算建议」小节（一两句话），"
       "并把第一个小标题下面那句话改得更具体一点。别新建页面。")


async def run():
    async with websockets.connect(BASE.replace("http", "ws", 1) + "/ws", max_size=None) as ws:
        await ws.send(json.dumps({"type": "send", "chat_id": gid, "text": msg}))
        tools, end = [], time.time() + 400
        while time.time() < end:
            try:
                d = json.loads(await asyncio.wait_for(ws.recv(), 30))
            except asyncio.TimeoutError:
                if ("event", "bot:1") in tools:   # Nova 改过页面、又安静了 30 秒
                    break
                continue
            if d.get("type") == "message" and d.get("chat_id") == gid:
                m = d.get("message", {})
                meta = m.get("meta") or {}
                if meta.get("kind") == "tool" or meta.get("tool"):
                    tools.append((d.get("bot_id"), meta.get("tool") or meta.get("name")))
                print("msg", d.get("bot_id"), m.get("role"), meta.get("kind"), (m.get("content") or "")[:80].replace("\n", " "))
            if d.get("type") == "page_changed":
                print("page_changed", d["page_id"], d["version"], d["by"], d.get("changed"))
                tools.append(("event", d["by"]))
        return tools

asyncio.run(run())
time.sleep(2)
new = [p for p in api("/api/pages") if p["id"] not in before]
print("新页面：", [(p["id"], p["title"], p["bot_id"]) for p in new])
for p in new:
    b = api(f"/api/pages/{p['id']}/blocks")
    print("段的作者：", [(x["by"], x["md"][:30]) for x in b["blocks"]])
    print("历史：", [(v["version"], v["by"], v["label"]) for v in api(f"/api/pages/{p['id']}/versions")])
