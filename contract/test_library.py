"""资料库：页面、小应用的数据、分享、搜索、头像、对话图片、链接卡片、Bot 的资料库、置顶文件。"""
import base64
import json
import struct
import zlib

from conftest import until
from fakellm_http import Call


def png(w=3, h=2) -> bytes:
    raw = b"".join(b"\x00" + b"\xdc\x1e\x1e" * w for _ in range(h))

    def chunk(t, data):
        return struct.pack(">I", len(data)) + t + data + struct.pack(">I", zlib.crc32(t + data) & 0xFFFFFFFF)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def test_library_pages_crud_and_share(srv):
    b = srv.make_bot("Librarian")
    p = srv.post("/api/pages", {"bot_id": b["id"], "title": "  周报   第一期 ", "content": "# 标题\n\n正文"})
    assert p["title"] == "周报 第一期" and p["format"] == "markdown" and p["app"] is None and p["size"] == 8
    assert "content" not in p
    got = srv.get(f"/api/pages/{p['id']}")
    assert got["content"] == "# 标题\n\n正文"
    assert any(x["id"] == p["id"] for x in srv.get("/api/pages"))
    r = srv.patch(f"/api/pages/{p['id']}", {"title": "", "pinned": True})
    assert r["pinned"] is True and r["title"] == "未命名页面"
    assert srv.get("/api/pages")[0]["id"] == p["id"]   # 置顶的在前
    s = srv.post(f"/api/pages/{p['id']}/share", {"on": True})
    assert s["share_token"]
    shared = srv.get(f"/api/shared/{s['share_token']}")
    assert shared["content"] == "# 标题\n\n正文" and shared["bot"]["name"] == "Librarian"
    assert set(shared) == {"title", "format", "content", "updated", "bot"}
    assert srv.http.get(f"/api/pages/{p['id']}/raw").status_code == 404   # markdown 没有原文地址
    srv.post(f"/api/pages/{p['id']}/share", {"on": False})
    r = srv.http.get(f"/api/shared/{s['share_token']}")
    assert r.status_code == 404 and r.json()["detail"] == "链接已失效"
    assert srv.http.post("/api/pages", json={"title": "x", "content": "  "}).json()["detail"] == "页面内容不能为空"
    assert srv.http.get("/api/pages/999999").status_code == 404
    srv.delete(f"/api/pages/{p['id']}")
    assert srv.http.get(f"/api/pages/{p['id']}").status_code == 404


def test_library_applet_raw_and_data(srv):
    b = srv.make_bot("Builder")
    p = srv.post("/api/pages", {"bot_id": b["id"], "title": "表单", "format": "html",
                                "content": "<html><head><title>t</title></head><body>hi</body></html>",
                                "app": {"tools": ["web_search", " web_search ", ""], "chat": 1, "junk": 2}})
    assert p["app"] == {"tools": ["web_search"], "chat": True}
    r = srv.http.get(f"/api/pages/{p['id']}/raw")
    assert r.status_code == 200 and "sandbox allow-scripts" in r.headers["content-security-policy"]
    assert r.headers["referrer-policy"] == "no-referrer"
    assert r.text.startswith("<html><head><script>") and "window.osa" in r.text and r.text.endswith("<body>hi</body></html>")
    # 数据
    assert srv.get(f"/api/pages/{p['id']}/data") == {}
    d = srv.put(f"/api/pages/{p['id']}/data", {"b": {"x": [1, "二"]}, "a": 1})
    assert d == {"b": {"x": [1, "二"]}, "a": 1}
    assert srv.get(f"/api/pages/{p['id']}/data") == {"a": 1, "b": {"x": [1, "二"]}}
    assert srv.put(f"/api/pages/{p['id']}/data", {"a": None}) == {"b": {"x": [1, "二"]}}
    big = srv.http.put(f"/api/pages/{p['id']}/data", json={"big": "x" * 40000})
    assert big.status_code == 400 and "太大" in big.json()["detail"]
    assert srv.http.put(f"/api/pages/{p['id']}/data", json=[1]).status_code in (400, 422)
    # 更新时不给 app 就保留原来的清单（工具接口）
    md = srv.post("/api/pages", {"bot_id": b["id"], "title": "普通", "content": "x"})
    r = srv.http.get(f"/api/pages/{md['id']}/data")
    assert r.status_code == 400 and r.json()["detail"] == f"页面 #{md['id']} 不是小应用"
    # 小应用调工具：清单里没有的不行，chat 为假时不能发消息
    r = srv.http.post(f"/api/pages/{p['id']}/call", json={"tool": "fetch_url", "args": {}})
    assert r.status_code == 403
    q = srv.post("/api/pages", {"bot_id": b["id"], "title": "无聊天", "format": "html", "content": "<p>x</p>", "app": {"tools": []}})
    r = srv.http.post(f"/api/pages/{q['id']}/say", json={"text": "你好"})
    assert r.status_code == 403
    r = srv.http.post(f"/api/pages/{p['id']}/say", json={"text": "   "})
    assert r.status_code == 400 and r.json()["detail"] == "消息不能为空"
    raw = srv.http.get(f"/api/pages/{q['id']}/raw").text
    assert raw.startswith("<script>") and raw.endswith("<p>x</p>")
    s = srv.post(f"/api/pages/{p['id']}/share", {"on": True})
    assert "window.osa" in srv.http.get(f"/api/shared/{s['share_token']}/raw").text


def test_library_applet_say_reaches_bot(srv, fake, ws):
    b = srv.make_bot("Talker")
    p = srv.post("/api/pages", {"bot_id": b["id"], "title": "面板", "format": "html", "content": "<p>x</p>",
                                "app": {"tools": [], "chat": True}})
    fake.script("router:Talker", "收到小应用的消息")
    srv.post(f"/api/pages/{p['id']}/say", {"text": "按钮点了"})
    until(lambda: srv.find(b["chat_id"], role="assistant", text="收到小应用的消息"), what="Bot 回复")
    u = srv.find(b["chat_id"], role="user", text="按钮点了")[0]
    assert u["meta"]["applet"] == {"id": p["id"], "title": "面板"}
    seen = fake.bodies("router", "Talker")[0]["messages"]
    assert any("（来自小应用「面板」）按钮点了" in str(m.get("content")) for m in seen)


def test_library_tools_save_read_data_search(srv, fake, ws):
    b = srv.make_bot("Writer2")
    fake.script("router:Writer2",
                Call("save_page", {"title": "猫的报告", "content": "猫是一种可爱的动物。\n\n橘猫特别能吃。", "format": "markdown"}),
                "回复甲")
    ws.say(b["chat_id"], "做个页面")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="回复甲"), what="回复")
    tool = [m for m in fake.bodies("router", "Writer2")[-1]["messages"] if m["role"] == "tool"][0]["content"]
    assert tool.startswith("已保存页面 #") and "「猫的报告」，链接 /library/pages/" in tool
    pid = int(tool.split("#")[1].split("「")[0])
    assert until(lambda: ws.of("pages_changed"), what="pages_changed")

    fake.script("router:Writer2", Call("read_page", {"page_id": pid}), "回复乙")
    ws.say(b["chat_id"], "读一下")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="回复乙"), what="回复")
    tool = [m for m in fake.bodies("router", "Writer2")[-1]["messages"] if m["role"] == "tool"][0]["content"]
    assert tool.startswith(f"《猫的报告》/library/pages/{pid}，") and tool.endswith("更新\n\n猫是一种可爱的动物。\n\n橘猫特别能吃。")

    fake.script("router:Writer2", Call("read_page", {"page_id": 999999}), Call("page_data", {"page_id": pid}),
                Call("search_memory", {"query": "橘猫"}), "回复丙")
    ws.say(b["chat_id"], "再看看")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="回复丙"), what="回复")
    outs = [m["content"] for m in fake.bodies("router", "Writer2")[-1]["messages"] if m["role"] == "tool"]
    assert outs[0] == "没有页面 #999999"
    assert outs[1] == f"页面 #{pid} 不是小应用，没有数据。要做成小应用，用 save_page 更新它并填 app"
    assert outs[2].startswith(f"- 页面 #{pid}《猫的报告》/library/pages/{pid}：")

    # 小应用：工具写数据，接口读到
    fake.script("router:Writer2",
                Call("save_page", {"title": "看板", "content": "<div>b</div>", "format": "html", "app": {"tools": ["no_such_tool"]}}),
                Call("save_page", {"title": "看板", "content": "<div>b</div>", "format": "html", "app": {"tools": [], "chat": False}}),
                "回复丁")
    ws.say(b["chat_id"], "做个看板")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="回复丁"), what="回复")
    outs = [m["content"] for m in fake.bodies("router", "Writer2")[-1]["messages"] if m["role"] == "tool"]
    assert outs[0] == "没保存：app.tools 里有现在没有的工具 no_such_tool，只能列你这一轮能用的工具"
    assert outs[1].startswith("已保存小应用 #")
    aid = int(outs[1].split("#")[1].split("「")[0])
    fake.script("router:Writer2", Call("page_data", {"page_id": aid, "set": {"待办": ["买猫粮"], "n": 2}}), Call("page_data", {"page_id": aid}), "回复戊")
    ws.say(b["chat_id"], "写数据")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="回复戊"), what="回复")
    outs = [m["content"] for m in fake.bodies("router", "Writer2")[-1]["messages"] if m["role"] == "tool"]
    assert outs[0] == '已写入 2 个键。小应用「看板」的数据（2 个键）：\n{\n "待办": [\n  "买猫粮"\n ],\n "n": 2\n}'
    assert outs[1] == '小应用「看板」的数据（2 个键）：\n{\n "n": 2,\n "待办": [\n  "买猫粮"\n ]\n}'
    assert srv.get(f"/api/pages/{aid}/data") == {"n": 2, "待办": ["买猫粮"]}
    assert until(lambda: [e for e in ws.of("page_data") if e["page_id"] == aid], what="page_data 事件")
    # 别人的小应用只能读
    other = srv.make_bot("Other2")
    fake.script("router:Other2", Call("page_data", {"page_id": aid, "set": {"x": 1}}), "回复己")
    ws.say(other["chat_id"], "改别人的")
    until(lambda: srv.find(other["chat_id"], role="assistant", text="回复己"), what="回复")
    out = [m["content"] for m in fake.bodies("router", "Other2")[-1]["messages"] if m["role"] == "tool"][0]
    assert out == f"没写：小应用 #{aid} 不是你建的，只能读"

    # Bot 的资料库：页面和回复里的链接
    fake.script("router:Writer2", "参考 [官方文档](https://example.com/docs)。以及 https://foo.example.org/a，还有 /library/pages/1")
    ws.say(b["chat_id"], "给个链接")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="官方文档"), what="回复")
    outs = srv.get(f"/api/bots/{b['id']}/outputs")
    kinds = [(o["kind"], o.get("url") or o.get("title")) for o in outs]
    assert ("link", "https://example.com/docs") in kinds and ("link", "https://foo.example.org/a") in kinds
    assert ("page", "猫的报告") in kinds and ("page", "看板") in kinds
    link = next(o for o in outs if o.get("url") == "https://example.com/docs")
    assert link["title"] == "官方文档" and link["host"] == "example.com" and link["count"] == 1 and link["key"] == "link:https://example.com/docs"
    assert srv.http.get("/api/bots/999999/outputs").status_code == 404


def test_library_search_api(srv):
    b = srv.make_bot("Searcher")
    srv.post("/api/pages", {"bot_id": b["id"], "title": "旅行计划", "content": "下个月去京都看红叶。\n\n住在岚山附近。"})
    srv.post(f"/api/bots/{b['id']}/memories", {"text": "用户喜欢京都的红叶"})
    r = srv.get("/api/search", params={"q": "京都红叶"})
    kinds = {h["kind"] for h in r["hits"]}
    assert kinds == {"page", "memory"}
    page = next(h for h in r["hits"] if h["kind"] == "page")
    assert page["title"] == "旅行计划" and "京都" in page["snippet"] and page["score"] > 0
    assert r["info"]["embedding"] is None and r["info"]["chunks"] >= 2
    only = srv.get("/api/search", params={"q": "京都", "kind": "memory"})
    assert {h["kind"] for h in only["hits"]} == {"memory"}
    assert srv.get("/api/search", params={"q": "  "})["hits"] == []
    info = srv.get("/api/search/info")
    assert set(info) == {"embedding", "chunks", "embedded", "error"}


def test_library_avatars(srv):
    data = "data:image/png;base64," + base64.b64encode(png()).decode()
    r = srv.post("/api/avatars", {"data": data})
    assert r["image"].startswith("/avatars/") and len(r["image"]) == len("/avatars/") + 24
    f = srv.http.get(r["image"])
    assert f.status_code == 200 and f.headers["content-type"] == "image/png" and f.content == png()
    assert "immutable" in f.headers["cache-control"] and f.headers["content-security-policy"] == "default-src 'none'"
    assert srv.http.get("/avatars/" + "0" * 24).status_code == 404
    bad = srv.http.post("/api/avatars", json={"data": "data:image/png;base64,aGVsbG8="})
    assert bad.status_code == 400 and bad.json()["detail"] == "只支持 PNG、JPG、WebP 图片"
    assert srv.http.post("/api/avatars", json={}).json()["detail"] == "没收到图片"
    b = srv.make_bot("Pretty")
    u = srv.patch(f"/api/bots/{b['id']}", {"avatar": {"shape": "star", "color": "#123456", "image": r["image"]}})
    assert u["avatar"]["image"] == r["image"]
    m = srv.get("/api/avatars/models")
    assert set(m) >= {"models", "default", "edit_models", "edit_default"}
    assert srv.http.post("/api/avatars/generate", json={"prompt": " "}).json()["detail"] == "写一句想要什么样的头像"


def test_library_media(srv):
    r = srv.http.put("/api/media", params={"name": "../x/截图.png"}, content=png(5, 4))
    assert r.status_code == 200, r.text
    m = r.json()
    assert m["mime"] == "image/png" and (m["w"], m["h"]) == (5, 4) and m["name"] == "截图.png" and len(m["id"]) == 24
    f = srv.http.get(f"/api/media/{m['id']}")
    assert f.content == png(5, 4) and f.headers["content-type"] == "image/png"
    assert f.headers["content-disposition"] == "inline; filename*=UTF-8''%E6%88%AA%E5%9B%BE.png"
    assert "private" in f.headers["cache-control"]
    d = srv.http.get(f"/api/media/{m['id']}", params={"download": "true"})
    assert d.headers["content-disposition"].startswith("attachment;")
    assert srv.http.get(f"/media/{m['id']}").content == png(5, 4)
    assert srv.http.get("/api/media/zzz").status_code == 404
    bad = srv.http.put("/api/media", content=b"not an image")
    assert bad.status_code == 400 and bad.json()["detail"] == "只支持 PNG、JPG、WebP、GIF 图片"
    assert srv.http.get(f"/pub/media/{m['id']}", params={"exp": 9999999999, "sig": "x"}).status_code == 403
    # 视频按 Range 分段
    video = b"\x00\x00\x00\x18ftypmp42" + bytes(range(100))
    import sqlite3
    c = sqlite3.connect(srv.data / "osa.db", timeout=5)
    c.execute("insert into media(id, mime, name, size, w, h, data, created) values(?,?,?,?,?,?,?,?)",
              ("ab" * 12, "video/mp4", "v.mp4", len(video), 0, 0, video, 9e9))
    c.commit()
    c.close()
    full = srv.http.get("/api/media/" + "ab" * 12)
    assert full.status_code == 200 and full.content == video and full.headers["accept-ranges"] == "bytes"
    part = srv.http.get("/api/media/" + "ab" * 12, headers={"Range": "bytes=2-5"})
    assert part.status_code == 206 and part.content == video[2:6] and part.headers["content-range"] == f"bytes 2-5/{len(video)}"
    tail = srv.http.get("/api/media/" + "ab" * 12, headers={"Range": "bytes=-4"})
    assert tail.status_code == 206 and tail.content == video[-4:]
    over = srv.http.get("/api/media/" + "ab" * 12, headers={"Range": "bytes=99999-"})
    assert over.status_code == 416 and over.headers["content-range"] == f"bytes */{len(video)}"


def test_library_unfurl_and_pins(srv):
    assert srv.get("/api/unfurl", params={"url": "http://127.0.0.1:9/x"}) == {"ok": False}
    assert srv.get("/api/unfurl", params={"url": "ftp://example.com"}) == {"ok": False}
    assert srv.get("/api/unfurl") == {"ok": False}
    r = srv.post("/api/files/pin", {"path": "报告/a.txt", "on": True})
    assert r == {"ok": True, "pins": ["/data/workspace/报告/a.txt"]}
    r = srv.post("/api/files/pin", {"path": "/data/workspace/报告", "on": False})
    assert r["pins"] == []
    bad = srv.http.post("/api/files/pin", json={"path": "/etc/passwd", "on": True})
    assert bad.status_code == 400 and bad.json()["detail"] == "只能访问 /data/workspace 里的文件"


def test_library_json_roundtrip_in_db(srv):
    """存进数据库的 JSON 和 Python 版格式一样（换回 Python 版也能读）。"""
    b = srv.make_bot("Fmt")
    p = srv.post("/api/pages", {"bot_id": b["id"], "title": "t", "format": "html", "content": "<p/>", "app": {"tools": [], "chat": True}})
    srv.put(f"/api/pages/{p['id']}/data", {"k": {"中": 1}})
    with srv.db() as c:
        app = c.execute("select app from pages where id=?", (p["id"],)).fetchone()[0]
        val = c.execute("select value from page_data where page_id=?", (p["id"],)).fetchone()[0]
    assert app == '{"tools": [], "chat": true}'
    assert val == '{"中": 1}'
    assert json.loads(val) == {"中": 1}
