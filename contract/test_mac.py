"""你的 Mac：派任务到 Mac（/api/computer/links、/api/computer/macs…、/api/computer/link 的连接），本机电脑（host）。

“Mac”是测试里另起的一个进程，按桌面 App 的办法跑小程序（Python 版 `python -m osa.computer.agent`，Rust 版 `osa-server agent`），
主动连到被测的服务器。两种小程序都连一遍：Python 版的服务器 ⇄ 两种小程序、Rust 版的服务器 ⇄ 两种小程序，线上的消息要互通。
小程序的工作文件夹在临时目录里；它跑命令、读写文件都在那里面。终端每条命令都弹卡片，测试替你点同意。
不碰整个屏幕的真实鼠标键盘：只看权限的检查（电脑面板上的 desktop 一栏）。
本机电脑（host）：装了 Chrome 才测，用临时的配置目录（不碰你自己的 Chrome），开起来、取一张画面、关掉。
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import httpx
import pytest

import conftest
from conftest import ROOT, RUST_BIN, Server, Socket, idle, make_config, until
from fakellm_http import Call

AGENTS = ["python", "rust"]


class Agent:
    """你的 Mac 上的小程序。"""

    def __init__(self, impl: str, srv: Server, token: str):
        self.tmp = Path(tempfile.mkdtemp(prefix=f"osa-agent-{impl}-"))
        self.data = self.tmp / "data"
        self.data.mkdir()
        cfg = make_config(self.tmp, "http://127.0.0.1:9/v1")
        env = {k: v for k, v in os.environ.items() if k not in ("INSTABOX_API_KEY", "OSA_PASSWORD", "OSA_VAULT_KEY", "OSA_DESKTOP")}
        env |= {"OSA_LINK_URL": srv.url.replace("http", "ws") + "/api/computer/link", "OSA_LINK_TOKEN": token,
                "OSA_DATA_DIR": str(self.data), "OSA_CONFIG": str(cfg), "OSA_ROOT": str(ROOT), "NO_PROXY": "*", "no_proxy": "*"}
        cmd = [RUST_BIN, "agent"] if impl == "rust" else [sys.executable, "-m", "osa.computer.agent"]
        self.log = open(self.tmp / "agent.log", "w")
        self.proc = subprocess.Popen(cmd, cwd=ROOT, env=env, stdout=self.log, stderr=subprocess.STDOUT)
        self.workspace = (self.data / "workspace").resolve()

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(15)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.log.close()
        shutil.rmtree(self.tmp, ignore_errors=True)


@pytest.fixture(scope="module")
def msrv(llm):
    s = Server(llm, config={"computer": {"enabled": True, "backend": "local", "width": 1280, "height": 800}})
    yield s
    s.stop()


def _macs(srv):
    return srv.get("/api/computer/macs")


@pytest.mark.parametrize("agent_impl", AGENTS)
def test_link_and_dispatch(msrv, fake, agent_impl):
    if agent_impl == "rust" and not Path(RUST_BIN).exists():
        pytest.skip("没有构建 Rust 版")
    srv = msrv
    ws = Socket(srv)
    link = srv.post("/api/computer/links", {"name": "TestMac"})
    assert link["id"] and len(link["token"]) >= 40
    ag = Agent(agent_impl, srv, link["token"])
    try:
        lid = link["id"]
        mac = until(lambda: next((m for m in _macs(srv) if m["id"] == lid and m["connected"]), None), 30, "Mac 连上")
        assert mac["can_shell"] is True and mac["shell"] is False and mac["controller"] == "bot"
        assert "desktop" in mac and "supported" in mac["desktop"]
        if mac["desktop"]["supported"]:
            assert set(mac["desktop"]) >= {"supported", "paused_by", "owner", "problem"}
        assert until(lambda: [e for e in ws.of("macs") if any(m["id"] == lid and m["connected"] for m in e["macs"])], 10, "macs 事件")
        # 终端默认关；在电脑面板里打开
        st = srv.put(f"/api/computer/macs/{lid}", {"shell": True})
        assert st["shell"] is True and st["can_shell"] is True

        a = srv.make_bot(f"MacBot{agent_impl}")
        fake.script(f"router:MacBot{agent_impl}", Call("dispatch", {"goal": "在 Mac 上记一个笔记", "mac": f"#{lid}"}), "派到你的 Mac 上了")
        fake.script("worker", Call("computer_shell", {"cmd": "echo hello-mac > note.txt && cat note.txt"}),
                    Call("computer_read_file", {"path": "note.txt"}), "记好了")
        ws.say(a["chat_id"], "在我的 Mac 上记个笔记")
        card = until(lambda: [e for e in ws.of("approval") if e.get("status") == "pending" and e["tool"] == "use_mac"], 20, "派到 Mac 的卡片")[0]
        assert card["chat_id"] == a["chat_id"] and card["mac"] == lid
        ws.send({"type": "approve", "id": card["id"], "ok": True})
        shell = until(lambda: [e for e in ws.of("approval") if e.get("status") == "pending" and e["tool"] == "computer_shell"], 20, "终端命令的卡片")[0]
        assert shell["mac"] == lid and shell["prefix"] == "echo"
        ws.send({"type": "approve", "id": shell["id"], "ok": True})
        res = until(lambda: srv.find(a["chat_id"], kind="result"), 30, "任务结果")[0]
        assert "记好了" in res["content"]
        outs = [m["content"] for b in fake.bodies("worker") for m in b["messages"] if m["role"] == "tool"]
        assert any("退出码 0" in o and "hello-mac" in o for o in outs), outs
        assert any("hello-mac" in o and "退出码" not in o for o in outs), outs
        assert (ag.workspace / "note.txt").read_text() == "hello-mac\n"
        sys_prompt = fake.bodies("worker")[0]["messages"][0]["content"]
        assert "这个任务派到了用户的 Mac" in sys_prompt
        # 那台 Mac 上的文件：下载
        r = srv.http.get("/api/files/download", params={"path": str(ag.workspace / "note.txt"), "mac": lid})
        assert r.status_code == 200 and r.content == b"hello-mac\n"
        r = srv.http.get("/api/files/download", params={"path": str(ag.workspace / "note.txt"), "mac": lid},
                         headers={"range": "bytes=0-4"})
        assert r.status_code == 206 and r.content == b"hello"
        assert srv.http.get("/api/files/download", params={"path": "/etc/hosts", "mac": lid}).status_code == 400
        idle(srv)
        # 接手 / 交还
        st = srv.post(f"/api/computer/macs/{lid}/takeover")
        assert st["controller"] == "human"
        st = srv.post(f"/api/computer/macs/{lid}/handback")
        assert st["controller"] == "bot"
        assert srv.http.post(f"/api/computer/macs/{lid}/nope").status_code == 404
        # 移除：令牌作废，小程序断开
        assert srv.delete(f"/api/computer/links/{lid}") == {"ok": True}
        assert not [m for m in _macs(srv) if m["id"] == lid]
        assert srv.http.put(f"/api/computer/macs/{lid}", json={"shell": False}).status_code == 404
    finally:
        ag.stop()
        ws.close()


def test_pick_mac_messages(msrv, fake):
    srv = msrv
    ws = Socket(srv)
    try:
        a = srv.make_bot("NoMacBot")
        fake.script("router:NoMacBot", Call("dispatch", {"goal": "做点事", "mac": "不存在的 Mac"}), "好")
        ws.say(a["chat_id"], "派到 Mac")
        until(lambda: srv.find(a["chat_id"], role="assistant"), 15, "回复")
        tool = [m for m in fake.bodies("router", "NoMacBot")[-1]["messages"] if m["role"] == "tool"][0]["content"]
        assert tool.startswith("没派发：没有叫「不存在的 Mac」的 Mac。用户登记过的 Mac：")
        # 登记了但没连上
        lid = srv.post("/api/computer/links", {"name": "Offline Mac"})["id"]
        fake.script("router:NoMacBot", Call("dispatch", {"goal": "做点事", "mac": "offline mac"}), "好")
        ws.say(a["chat_id"], "再派一次")
        until(lambda: len(srv.find(a["chat_id"], role="assistant")) >= 2, 15, "第二次回复")
        tool = [m for m in fake.bodies("router", "NoMacBot")[-1]["messages"] if m["role"] == "tool"][0]["content"]
        assert tool == "没派发：「Offline Mac」现在没连上（那台 Mac 上的 App 没开、没联网，或者电脑睡着了）。请告诉用户。"
        sys_prompt = fake.bodies("router", "NoMacBot")[-1]["messages"][0]["content"]
        assert "- 用户的 Mac：「Offline Mac」（没连上）" in sys_prompt
        assert srv.delete(f"/api/computer/links/{lid}") == {"ok": True}
    finally:
        ws.close()


def test_link_rejects_bad_token(msrv):
    from websockets.sync.client import connect
    from websockets.exceptions import ConnectionClosed
    ws = connect(msrv.url.replace("http", "ws") + "/api/computer/link", additional_headers={"Authorization": "Bearer nope"},
                 open_timeout=10, proxy=None)
    with pytest.raises(ConnectionClosed) as e:
        ws.recv(timeout=10)
    assert e.value.rcvd.code == 4401


def _chrome() -> bool:
    return sys.platform == "darwin" and Path("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome").exists()


@pytest.mark.skipif(not _chrome(), reason="不是装了 Chrome 的 Mac")
def test_host_chrome(llm):
    """本机电脑：Bot 用的 Chrome（临时的配置目录）开起来、取画面、关掉。"""
    s = Server(llm, config={"computer": {"enabled": True, "backend": "host", "host_desktop": True, "host_shell": False}})
    try:
        bot = s.get("/api/bootstrap")["bots"][0]["id"]
        st = s.get(f"/api/bots/{bot}/computer")
        assert st["backend"] == "host" and st["state"] in ("stopped", "running") and st["shell"] is False
        assert st["workspace"] == str((s.data / "workspace").resolve())
        assert st["desktop"]["supported"] is True and set(st["desktop"]) >= {"accessibility", "screen", "problem", "owner"}
        st = s.post(f"/api/bots/{bot}/computer/start")
        assert st["state"] == "running"
        assert (s.data / "computers" / "host" / "chrome" / "DevToolsActivePort").exists()
        r = s.http.get(f"/api/bots/{bot}/computer/shot", params={"view": "chrome"})
        assert r.status_code == 200 and r.content[:3] == b"\xff\xd8\xff"
        st = s.post(f"/api/bots/{bot}/computer/pause")
        assert st["state"] == "stopped"
    finally:
        try:
            bot = s.get("/api/bootstrap")["bots"][0]["id"]
            s.http.post(f"/api/bots/{bot}/computer/pause")
        except Exception:
            pass
        s.stop()
