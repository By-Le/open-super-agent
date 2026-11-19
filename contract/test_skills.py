"""技能、Bot 模板、主 Bot 管理 Bot（skills.py、templates.py、botadmin.py）的接口测试。"""
import base64
import io
import json
import zipfile

from conftest import idle, until
from fakellm_http import Call


def primary(srv) -> dict:
    return next(b for b in srv.get("/api/bootstrap")["bots"] if b["is_primary"])


def tool_outputs(fake, role: str, name: str = "") -> list[str]:
    """各次请求里出现过的工具结果，按出现顺序去重（后面的请求会带着前面的）。"""
    out: dict[str, None] = {}
    for b in fake.bodies(role, name):
        for m in b["messages"]:
            if m["role"] == "tool":
                out.setdefault(m["tool_call_id"] + "\0" + m["content"], None)
    return [k.split("\0", 1)[1] for k in out]


def test_skill_crud_export_share(srv):
    b = srv.make_bot("Skiller")
    s = srv.post(f"/api/bots/{b['id']}/skills", {"name": "周报", "description": "写周报", "body": "1. 汇总\n2. 写",
                                                 "files": {"scripts/a.py": "print(1)"}})
    assert s["name"] == "周报" and s["bot_id"] == b["id"] and s["source"] == "manual"
    assert s["files"] == {"scripts/a.py": "print(1)"} and s["apps"] is None
    assert [x["name"] for x in srv.get(f"/api/bots/{b['id']}/skills")] == ["周报"]
    # 同名再建：覆盖
    s2 = srv.post(f"/api/bots/{b['id']}/skills", {"name": "周报", "body": "新的做法"})
    assert s2["id"] == s["id"] and s2["body"] == "新的做法" and s2["files"] == {}
    # 改：files 没传保持原样
    srv.put(f"/api/skills/{s['id']}", {"files": {"r.md": "x"}, "apps": ["@Notion", "gmail", "Bad App"]})
    s3 = srv.put(f"/api/skills/{s['id']}", {"description": "新说明"})
    assert s3["description"] == "新说明" and s3["files"] == {"r.md": "x"} and s3["apps"] == ["notion", "gmail"]
    # 名字不对
    r = srv.http.post(f"/api/bots/{b['id']}/skills", json={"name": "有 空格", "body": "x"})
    assert r.status_code == 400 and "技能名" in r.json()["detail"]
    r = srv.http.post(f"/api/bots/{b['id']}/skills", json={"name": "ok", "body": "x", "files": {"../a": "x"}})
    assert r.status_code == 400
    # 导出：带文件的是 zip
    r = srv.http.get(f"/api/skills/{s['id']}/export")
    assert r.status_code == 200 and r.headers["content-type"].startswith("application/zip")
    z = zipfile.ZipFile(io.BytesIO(r.content))
    assert sorted(z.namelist()) == ["SKILL.md", "r.md"]
    md = z.read("SKILL.md").decode()
    assert md.startswith("---\nname: 周报\n") and "新的做法" in md
    # 分享
    sh = srv.post(f"/api/skills/{s['id']}/share", {"on": True})
    tok = sh["share_token"]
    assert tok and srv.post(f"/api/skills/{s['id']}/share", {"on": True})["share_token"] == tok
    pub = srv.get(f"/api/shared/skills/{tok}")
    assert pub["name"] == "周报" and pub["files"] == ["r.md"] and pub["bot"]["name"] == "Skiller"
    assert srv.http.get(f"/api/shared/skills/{tok}/raw").status_code == 200
    srv.post(f"/api/skills/{s['id']}/share", {"on": False})
    assert srv.http.get(f"/api/shared/skills/{tok}").status_code == 404
    assert srv.http.post("/api/skills/999999/share", json={"on": True}).status_code == 404
    srv.delete(f"/api/skills/{s['id']}")
    assert srv.get(f"/api/bots/{b['id']}/skills") == []


def test_parse_text_and_zip(srv):
    b = srv.make_bot("Parser")
    d = srv.post(f"/api/bots/{b['id']}/skills/parse",
                 {"text": "---\nname: My Skill.v2\ndescription: |\n  多行\n  说明\napps: [notion, '@Exa']\n---\n\n# 标题\n做法正文"})
    assert d["name"] == "My-Skill-v2" and d["description"] == "多行 说明" and d["body"] == "# 标题\n做法正文"
    assert d["declared_apps"] == ["notion", "exa"] and d["skipped"] == [] and d["exists"] is False
    assert [a["name"] for a in d["apps"]] == ["notion", "exa"]
    # 没写名字：用一级标题
    d = srv.post(f"/api/bots/{b['id']}/skills/parse", {"text": "# 记账\n每天记一笔"})
    assert d["name"] == "记账" and d["declared_apps"] is None
    r = srv.http.post(f"/api/bots/{b['id']}/skills/parse", json={"text": "没有名字的内容"})
    assert r.status_code == 400
    # zip：文件夹里一层，带脚本和一张图（不收）
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("pack/SKILL.md", "---\ndescription: 打包的\n---\n做法")
        z.writestr("pack/scripts/run.py", "print('hi')")
        z.writestr("pack/logo.png", b"\x89PNG\r\n\x1a\n\x00\x00")
        z.writestr("pack/.DS_Store", "x")
    d = srv.post(f"/api/bots/{b['id']}/skills/parse", {"zip": base64.b64encode(buf.getvalue()).decode(), "filename": "pack.zip"})
    assert d["name"] == "pack" and d["files"] == {"scripts/run.py": "print('hi')"} and d["skipped"] == ["logo.png"]
    r = srv.http.post(f"/api/bots/{b['id']}/skills/parse", json={"zip": base64.b64encode(b"not a zip").decode()})
    assert r.status_code == 400


def test_market_install(srv):
    b = srv.make_bot("Marketer")
    m = srv.get(f"/api/bots/{b['id']}/skills/market")
    ids = [x["id"] for x in m]
    assert ids == sorted(ids) and "save-webpage" in ids and not any(x["installed"] for x in m)
    s = srv.post(f"/api/bots/{b['id']}/skills/market/save-webpage")
    assert s["source"] == "market" and s["apps"] == []
    assert next(x for x in srv.get(f"/api/bots/{b['id']}/skills/market") if x["id"] == "save-webpage")["installed"]
    assert srv.http.post(f"/api/bots/{b['id']}/skills/market/nope").status_code == 400


def test_slash_skill_and_save_skill(srv, fake, ws):
    b = srv.make_bot("Slasher")
    srv.post(f"/api/bots/{b['id']}/skills", {"name": "早报", "description": "做早报", "body": "搜新闻然后写早报"})
    fake.script("router:Slasher", "好的，这就做")
    ws.say(b["chat_id"], "/早报 只要科技")
    until(lambda: srv.find(b["chat_id"], role="assistant"), what="回复")
    user = srv.find(b["chat_id"], role="user")[-1]
    assert user["meta"]["skill"] == "早报" and "（用户调用了技能 /早报" in user["meta"]["skill_prompt"]
    assert user["meta"]["skill_prompt"].endswith("用户补充：只要科技")
    body = fake.bodies("router", "Slasher")[0]
    assert "- /早报：做早报" in body["messages"][0]["content"]
    last_user = [m for m in body["messages"] if m["role"] == "user"][-1]["content"]
    assert "--- 技能做法 ---\n搜新闻然后写早报\n--- 结束 ---" in last_user

    fake.script("router:Slasher", Call("save_skill", {"name": "复盘", "description": "复盘做法", "body": "1. 回顾\n2. 总结"}),
                Call("use_skill", {"name": "复盘"}), "存好了")
    ws.say(b["chat_id"], "把刚才的做法存成技能")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="存好了"), what="存好了")
    outs = tool_outputs(fake, "router", "Slasher")
    assert "已保存技能 /复盘" in outs
    assert any(o.startswith("（技能 /复盘 的做法，按它执行") for o in outs)
    assert any(x["name"] == "复盘" and x["source"] == "chat" for x in srv.get(f"/api/bots/{b['id']}/skills"))
    assert until(lambda: ws.of("skills_changed"), what="skills_changed")


def test_templates(srv):
    ts = srv.get("/api/templates")
    assert [t["id"] for t in ts] == ["github-watch", "mail", "news", "trip"]
    news = next(t for t in ts if t["id"] == "news")
    assert news["skills"] >= 1 and news["routines"] >= 1
    p = srv.post("/api/templates/preview", {"id": "news"})
    assert p["text"].startswith("# ") and "osa_template: 1" in p["text"]
    assert all({"name", "title", "connected", "in_market"} <= set(a) for a in p["apps"])
    b = srv.post("/api/templates/install", {"text": p["text"], "name": "我的资讯", "enable_routines": False})
    assert b["name"] == "我的资讯" and b["instructions"].startswith("你是我的资讯助手")
    sk = srv.get(f"/api/bots/{b['id']}/skills")
    assert [s["name"] for s in sk if s["bot_id"] == b["id"]] == [x["name"] for x in p["skills"]]
    assert all(s["source"] == "import" for s in sk if s["bot_id"] == b["id"])
    # 导出、分享
    r = srv.http.get(f"/api/bots/{b['id']}/template")
    assert r.status_code == 200 and "osa_template: 1" in r.text and ".osabot.yaml" in r.headers["content-disposition"]
    assert srv.get(f"/api/bots/{b['id']}/template/share") == {"token": None}
    tok = srv.post(f"/api/bots/{b['id']}/template/share", {"on": True})["token"]
    shared = srv.get(f"/api/shared/templates/{tok}")
    assert shared["name"] == "我的资讯" and all("connected" not in a for a in shared["apps"])
    assert srv.http.get(f"/api/shared/templates/{tok}/raw").status_code == 200
    again = srv.post("/api/templates/preview", {"text": srv.http.get(f"/api/shared/templates/{tok}/raw").text})
    assert again["name"] == "我的资讯"
    # 不对的模板
    for text, word in [("name: x", "osa_template"), ("osa_template: 2\nname: x", "升级"), ("osa_template: 1", "name"),
                       ("osa_template: 1\nname: x\nskills:\n  - {name: a, body: b}\n  - {name: a, body: c}", "两次"),
                       ("osa_template: 1\nname: x\napps: [Bad]", "apps")]:
        r = srv.http.post("/api/templates/preview", json={"text": text})
        assert r.status_code == 400 and word in r.json()["detail"], (text, r.text)
    assert srv.http.post("/api/templates/preview", json={"id": "../x"}).status_code == 400


def test_botadmin_create_update_delete(srv, fake, ws):
    nova = primary(srv)
    name = nova["name"]
    fake.script(f"router:{name}", Call("get_bot", {}), Call("create_bot", {"name": "Helper9", "instructions": "帮忙查资料。",
                                                                         "skills": [{"name": "查", "body": "去查"}]}),
                "卡片弹出来了，确认一下", "Helper9 建好了")
    ws.say(nova["chat_id"], "建一个帮忙查资料的 Bot")
    card = until(lambda: [e for e in ws.of("approval") if e.get("status") == "pending" and e["tool"] == "create_bot"], what="卡片")[0]
    assert card["chat_id"] == nova["chat_id"] and card["once"] is True and card["scopes"] == ["once"]
    assert card["editable"] == ["职责"] and json.loads(card["args"])["名字"] == "Helper9"
    outs = tool_outputs(fake, "router", name)
    assert outs[0].startswith("共 ") and outs[1].startswith("已弹出确认卡片（新建 Bot「Helper9」）")
    ws.send({"type": "approve", "id": card["id"], "ok": True, "edits": {"职责": "帮忙查资料，先给结论。"}})
    until(lambda: srv.find(nova["chat_id"], role="assistant", text="Helper9 建好了"), what="叫醒后的回复")
    done = srv.find(nova["chat_id"], kind="bot_created")
    assert done and done[0]["content"] == "[已新建 Bot「Helper9」]"
    h = next(b for b in srv.get("/api/bootstrap")["bots"] if b["name"] == "Helper9")
    assert h["instructions"] == "帮忙查资料，先给结论。" and done[0]["meta"]["new_bot"] == h["id"]
    assert [s["name"] for s in srv.get(f"/api/bots/{h['id']}/skills") if s["bot_id"] == h["id"]] == ["查"]
    woke = fake.bodies("router", name)[-1]
    last_user = [m for m in woke["messages"] if m["role"] == "user"][-1]["content"]
    assert "这一轮不是用户说的话" in last_user and "用户在卡片上改过职责才同意的" in last_user

    # 修改：用户拒绝（附了意见）
    fake.script(f"router:{name}", Call("update_bot", {"name": "Helper9", "new_name": "Helper10", "remove_skills": ["查"]}),
                "等你确认", "好的，不改了")
    ws.say(nova["chat_id"], "把 Helper9 改名")
    card = until(lambda: [e for e in ws.of("approval") if e.get("status") == "pending" and e["tool"] == "update_bot"], what="修改卡片")[0]
    args = json.loads(card["args"])
    assert args["新名字"] == "Helper10" and args["删技能"] == ["/查"]
    ws.send({"type": "approve", "id": card["id"], "ok": False, "note": "先别改"})
    until(lambda: srv.find(nova["chat_id"], role="assistant", text="好的，不改了"), what="拒绝后叫醒")
    assert srv.find(nova["chat_id"], kind="note", text="[没修改 Bot「Helper9」：先别改]")

    # 不能删主 Bot；删除走接口
    fake.script(f"router:{name}", Call("delete_bot", {"name": name}), "主 Bot 删不了")
    ws.say(nova["chat_id"], "删掉你自己")
    until(lambda: srv.find(nova["chat_id"], role="assistant", text="主 Bot 删不了"), what="回复")
    assert any(o.startswith("没删：主 Bot 不能删除") for o in tool_outputs(fake, "router", name))
    assert srv.http.delete(f"/api/bots/{nova['id']}").status_code == 400
    srv.delete(f"/api/bots/{h['id']}")
    assert not any(b["id"] == h["id"] for b in srv.get("/api/bootstrap")["bots"])
    with srv.db() as c:
        assert c.execute("select count(*) from skills where bot_id=?", (h["id"],)).fetchone()[0] == 0
    idle(srv)


def test_non_primary_update_self_only(srv, fake, ws):
    a = srv.make_bot("Selfie")
    fake.script("router:Selfie", Call("update_bot", {"name": primary(srv)["name"], "instructions": "x"}), "改不了")
    ws.say(a["chat_id"], "改主 Bot")
    until(lambda: srv.find(a["chat_id"], role="assistant", text="改不了"), what="回复")
    assert any(o.startswith("没改：你只能改你自己") for o in tool_outputs(fake, "router", "Selfie"))
    names = [t["function"]["name"] for t in fake.bodies("router", "Selfie")[0]["tools"]]
    assert "update_bot" in names and "create_bot" not in names
