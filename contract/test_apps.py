"""应用市场和应用：目录、从市场添加（密钥进保险库）、启用停用、删除、确认规则、保险库，以及 OAuth 登录授权的全过程。

OAuth 用一个本机的假服务（fake_oauth.py 里的 FakeOAuth）：受保护资源元数据、授权服务器元数据、动态注册、换令牌、刷新，
外加一个要 Bearer 令牌才能连的 MCP 端点。授权页不真的打开：从 /oauth/start 跳转的地址里取出 state，直接调回调。
"""
import base64
import hashlib
import json
import sys
import textwrap
import time
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import pytest
from cryptography.fernet import Fernet

from conftest import Server, Socket, idle, until
from fake_oauth import FakeOAuth
from fakellm_http import Call

HERE = Path(__file__).resolve().parent


@pytest.fixture(scope="module")
def oauth():
    f = FakeOAuth()
    yield f
    f.stop()


def manifest(srv: Server, name: str, text: str):
    d = srv.data / "apps"
    d.mkdir(exist_ok=True)
    (d / f"{name}.yaml").write_text(textwrap.dedent(text))


def vault_entry(srv: Server, name: str):
    key = (srv.data / "vault.key").read_text().strip()
    with srv.db() as c:
        r = c.execute("select ciphertext from vault where name=?", (name,)).fetchone()
    return json.loads(Fernet(key.encode()).decrypt(bytes(r[0]) if isinstance(r[0], (bytes, memoryview)) else r[0].encode())) if r else None


def app(srv: Server, name: str) -> dict | None:
    return next((a for a in srv.get("/api/apps")["apps"] if a["name"] == name), None)


def test_catalog_shape(srv):
    a = srv.get("/api/apps")
    assert a["oauth_callback"].endswith("/oauth/callback")
    cats = [c[0] for c in a["categories"]]
    assert cats[0] == "notes" and "files" in cats
    by = {i["id"]: i for i in a["catalog"]}
    assert len(by) >= 30
    fs, notion, gmail = by["filesystem"], by["notion"], by["gmail"]
    assert fs["kind"] == "none" and fs["remote"] is False and fs["fields"][0]["default"].endswith("/workspace")
    assert notion["kind"] == "oauth" and notion["remote"] is True and notion["client"] is False
    assert gmail["kind"] == "token"
    for it in a["catalog"]:
        assert not {"connect", "check", "order", "en"} & set(it)
        assert {"id", "title", "category", "desc", "fields", "example", "about", "remote", "kind", "can", "steps"} <= set(it)
        assert all("pattern" not in f and "nospace" not in f for f in it["fields"])
    # 按分类、order、名字排
    order = [c[0] for c in a["categories"]]
    ranks = [order.index(i["category"]) for i in a["catalog"]]
    assert ranks == sorted(ranks)


def test_local_manifest_errors_are_skipped(srv):
    manifest(srv, "broken", "title: 坏的\ncategory: nope\n")
    by = {i["id"] for i in srv.get("/api/apps")["catalog"]}
    assert "broken" not in by and "filesystem" in by


def test_install_from_market(srv, fake, ws):
    manifest(srv, "notetest", f"""
        title: 笔记测试
        category: notes
        by: 测试
        desc: 测试用的应用
        fields:
        - key: token
          label: 令牌
          secret: true
        connect:
          type: stdio
          command: {sys.executable}
          args: [{HERE / 'fake_mcp.py'}]
          env:
            NOTE_TOKEN: '{{token}}'
        """)
    it = next(i for i in srv.get("/api/apps")["catalog"] if i["id"] == "notetest")
    assert it["kind"] == "token"
    r = srv.http.post("/api/market/notetest", json={"values": {}})
    assert r.status_code == 400 and "令牌" in r.json()["detail"]
    a = srv.post("/api/market/notetest", {"values": {"token": " abc123 "}})
    assert a["state"] == "connected" and a["catalog"] == "notetest"
    assert set(a["tools"]) == {"search_notes", "create_note"}
    assert a["confirm_tools"] == ["create_note"]
    assert a["env"] == {"NOTE_TOKEN": "{vault:NOTETEST_TOKEN}"}
    assert any(v["name"] == "NOTETEST_TOKEN" and v["ok"] for v in srv.get("/api/vault"))
    assert ws.of("apps_changed")
    r = srv.http.post("/api/market/notetest", json={"values": {"token": "x"}})
    assert r.status_code == 400   # 已经添加过了

    # 停用、启用
    a = srv.post("/api/apps/notetest/disable")
    assert a["state"] == "off" and not a["tools"] and a["status"] == "未启用"
    a = srv.post("/api/apps/notetest/enable")
    assert a["state"] == "connected"
    assert srv.http.post("/api/apps/notetest/explode").status_code == 404
    assert srv.http.post("/api/apps/nosuch/enable").status_code == 404

    # 只改确认策略
    a = srv.patch("/api/apps/notetest", {"confirm": "none", "command": "evil"})
    assert a["confirm_tools"] == [] and a["command"] == sys.executable

    # 删除：市场替它存的密钥一起删
    srv.delete("/api/apps/notetest")
    assert app(srv, "notetest") is None
    assert not any(v["name"] == "NOTETEST_TOKEN" for v in srv.get("/api/vault"))


def test_custom_app_with_secret_and_token(srv):
    a = srv.put("/api/apps/custom1", {"command": sys.executable, "args": [str(HERE / "fake_mcp.py")],
                                      "secrets": {"API_KEY": "s3cret"}, "enabled": True})
    assert a["state"] == "connected" and a["env"] == {"API_KEY": "vault:CUSTOM1_API_KEY"}
    r = srv.http.put("/api/apps/Bad Name", json={"command": "x"})
    assert r.status_code == 400
    r = srv.http.put("/api/apps/nocmd", json={"command": ""})
    assert r.status_code == 400 and "启动命令" in r.json()["detail"]
    a = srv.put("/api/apps/remote1", {"type": "http", "url": "http://127.0.0.1:9/mcp", "token": "tok", "enabled": False})
    assert a["state"] == "off" and a["headers"] == ["Authorization"]
    names = {v["name"] for v in srv.get("/api/vault")}
    assert {"CUSTOM1_API_KEY", "REMOTE1_TOKEN"} <= names
    srv.delete("/api/apps/custom1")
    srv.delete("/api/apps/remote1")
    names = {v["name"] for v in srv.get("/api/vault")}
    assert not {"CUSTOM1_API_KEY", "REMOTE1_TOKEN"} & names


def test_vault(srv):
    assert srv.put("/api/vault/MY_KEY", {"value": "v1"}) == {"ok": True}
    assert any(v["name"] == "MY_KEY" and v["ok"] for v in srv.get("/api/vault"))
    assert srv.http.put("/api/vault/1bad", json={"value": "x"}).status_code == 400
    assert srv.http.put("/api/vault/EMPTY", json={"value": ""}).status_code == 400
    srv.delete("/api/vault/MY_KEY")
    assert not any(v["name"] == "MY_KEY" for v in srv.get("/api/vault"))


def test_rules_and_ask_rule(srv, fake, ws):
    until(lambda: (a := app(srv, "demo")) and a["state"] == "connected", timeout=30, what="假应用连上")
    assert srv.http.post("/api/rules", json={"pattern": " ", "action": "ask"}).status_code == 400
    assert srv.http.post("/api/rules", json={"pattern": "x", "action": "maybe"}).status_code == 400
    r = srv.post("/api/rules", {"pattern": "demo__search_*", "action": "ask", "note": "查之前先问"})
    assert r["pattern"] == "demo__search_*" and r["action"] == "ask"
    assert srv.post("/api/rules", {"pattern": "demo__search_*", "action": "ask"})["id"] == r["id"]   # 重复的不再加
    assert [x["id"] for x in srv.get("/api/rules")] == [r["id"]]

    b = srv.make_bot("Asker")
    fake.script("router:Asker", Call("dispatch", {"goal": "查笔记"}), "好")
    fake.script("worker", Call("demo__search_notes", {"query": "周报"}), "查到了")
    ws.say(b["chat_id"], "查一下笔记")
    card = until(lambda: [e for e in ws.of("approval") if e.get("status") == "pending" and e["tool"] == "demo__search_notes"],
                 what="规则要求的确认卡片")
    ws.send({"type": "approve", "id": card[0]["id"], "ok": False, "note": "先别查"})
    res = until(lambda: srv.find(b["chat_id"], kind="result"), what="任务结果")
    assert "查到了" in res[0]["content"]
    tool_out = [m for x in fake.bodies("worker") for m in x["messages"] if m["role"] == "tool"]
    assert any("用户拒绝了这个操作" in m["content"] and "先别查" in m["content"] for m in tool_out)
    idle(srv)
    srv.delete(f"/api/rules/{r['id']}")
    assert srv.get("/api/rules") == []


def test_oauth_login_flow(srv, oauth):
    oauth.reset()
    manifest(srv, "oauthtest", f"""
        title: 授权测试
        category: notes
        by: 测试
        desc: 要登录授权的测试应用
        connect:
          type: http
          url: {oauth.url}/mcp
          oauth: true
        """)
    oauth.expires_in = 10   # 令牌只有 10 秒：重连时（还剩不到 30 秒）先刷新
    r = srv.http.get("/oauth/callback?state=nope&code=x")
    assert "失效" in r.text
    r = srv.http.get("/oauth/start/oauthtest", headers={"sec-fetch-site": "cross-site"})
    assert r.status_code == 403
    r = srv.http.get("/oauth/start/oauthtest", headers={"sec-fetch-site": "same-origin"}, follow_redirects=False)
    assert r.status_code == 302, r.text[:500]
    loc = r.headers["location"]
    u = urlparse(loc)
    assert f"{u.scheme}://{u.netloc}{u.path}" == f"{oauth.url}/authorize"
    q = {k: v[0] for k, v in parse_qs(u.query).items()}
    assert q["response_type"] == "code" and q["client_id"] == "cid-1" and q["code_challenge_method"] == "S256"
    assert q["redirect_uri"] == srv.url + "/oauth/callback"
    assert q["resource"] == f"{oauth.url}/mcp" and q["scope"] == "docs.read"
    assert q["extra"] == "1"   # 授权地址自己带的参数（腾讯文档那种）：两个 ? 改成 &
    assert oauth.registered and oauth.registered[0]["redirect_uris"] == [srv.url + "/oauth/callback"]
    assert oauth.registered[0]["client_name"] == "Open Super Agent"
    a = app(srv, "oauthtest")
    assert a["state"] == "login"
    oauth.challenge = q["code_challenge"]

    # 回调：带回授权码，换到令牌，连上
    r = srv.http.get(f"/oauth/callback?state={q['state']}&code=code-1")
    assert r.status_code == 200 and "已连接" in r.text, r.text[:500]
    a = app(srv, "oauthtest")
    assert a["state"] == "connected" and a["tools"] == ["list_docs"]
    assert oauth.verifier_ok
    v = vault_entry(srv, "OAUTH_OAUTHTEST")
    assert v["tokens"]["access_token"] == "at-1" and v["tokens"]["token_type"] == "Bearer"
    assert v["tokens"]["refresh_token"] == "rt-1" and isinstance(v["expires_at"], float)
    assert v["client"]["client_id"] == "cid-1" and v["client"]["redirect_uris"] == [srv.url + "/oauth/callback"]

    # 同一个回调再来一次：已经失效
    r = srv.http.get(f"/oauth/callback?state={q['state']}&code=code-1")
    assert "失效" in r.text

    # 令牌快过期（还剩不到 30 秒）：重连时先用刷新令牌换新的，不用再登录
    oauth.next_token = "at-2"
    before = len(oauth.grants)
    a = srv.post("/api/apps/oauthtest/reconnect")
    assert a["state"] == "connected", a["status"]
    assert oauth.grants[before:] == ["refresh_token"], oauth.grants
    assert vault_entry(srv, "OAUTH_OAUTHTEST")["tokens"]["access_token"] == "at-2"
    assert vault_entry(srv, "OAUTH_OAUTHTEST")["tokens"]["refresh_token"] == "rt-1"   # 对方没换刷新令牌：沿用旧的

    # 令牌被对方作废、也刷新不了：要重新登录
    oauth.revoked = True
    oauth.refuse_refresh = True
    a = srv.post("/api/apps/oauthtest/reconnect")
    assert a["state"] == "needs_login", a
    assert "登录" in a["status"]

    # 取消正在进行的登录
    oauth.revoked = False
    oauth.refuse_refresh = False
    r = srv.http.get("/oauth/start/oauthtest", follow_redirects=False)
    assert r.status_code == 302
    q = {k: v[0] for k, v in parse_qs(urlparse(r.headers["location"]).query).items()}
    srv.post("/api/apps/oauthtest/cancel")
    until(lambda: app(srv, "oauthtest")["state"] != "login", what="取消后不再等登录")
    r = srv.http.get(f"/oauth/callback?state={q['state']}&code=code-1")
    assert "已连接" not in r.text
    srv.delete("/api/apps/oauthtest")
    assert vault_entry(srv, "OAUTH_OAUTHTEST") is None


def test_oauth_error_from_provider(srv, oauth):
    oauth.reset()
    manifest(srv, "oautherr", f"""
        title: 授权测试二
        category: notes
        by: 测试
        desc: 授权被拒绝
        connect:
          type: http
          url: {oauth.url}/mcp
          oauth: true
        """)
    r = srv.http.get("/oauth/start/oautherr", follow_redirects=False)
    q = {k: v[0] for k, v in parse_qs(urlparse(r.headers["location"]).query).items()}
    r = srv.http.get(f"/oauth/callback?state={q['state']}&error=access_denied&error_description=用户拒绝")
    assert "没有连上" in r.text and "用户拒绝" in r.text
    a = app(srv, "oautherr")
    assert a["state"] == "needs_login" and "用户拒绝" in a["status"]
    srv.delete("/api/apps/oautherr")


def test_mcp_apps_endpoints(srv):
    r = srv.http.post("/api/mcp-apps/nosuch/call", json={"name": "x", "arguments": {}})
    assert r.status_code == 404
    r = srv.http.post("/api/mcp-apps/nosuch/say", json={"text": "hi"})
    assert r.status_code == 404
    until(lambda: (a := app(srv, "demo")) and a["state"] == "connected", timeout=30, what="假应用连上")
    r = srv.http.post("/api/mcp-apps/demo/say", json={"text": "  "})
    assert r.status_code == 400
    r = srv.http.post("/api/mcp-apps/demo/call", json={"name": "search_notes", "arguments": {"query": "q"}})
    assert r.status_code == 200 and "找到 1 条笔记" in r.json()["content"][0]["text"]
    r = srv.http.get("/api/mcp-apps/demo/view", params={"uri": "ui://nope"})
    assert r.status_code == 404
