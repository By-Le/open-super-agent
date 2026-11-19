"""run_code / quick_code：在本机的受限子进程里跑代码（电脑关着，测试里 computer.enabled 是 false）。"""
import shutil
from pathlib import Path

import pytest

from conftest import idle, until
from fakellm_http import Call

_n = 0


def run_code(srv, fake, ws, args: dict) -> str:
    """让 Worker 调一次 run_code，返回工具的结果（给模型的文字）。"""
    global _n
    _n += 1
    b = srv.make_bot(f"Coder{_n}")
    fake.script(f"router:Coder{_n}", Call("dispatch", {"goal": "跑一段代码"}), "派了")
    fake.script("worker", Call("run_code", args), "完成")
    ws.say(b["chat_id"], "跑一段代码")
    until(lambda: srv.find(b["chat_id"], kind="result"), timeout=30, what="任务结果")
    idle(srv, 30)
    outs = [m["content"] for body in fake.bodies("worker") for m in body["messages"] if m["role"] == "tool"]
    assert outs, "Worker 没拿到工具结果"
    return outs[-1]


def test_python_output_and_files(srv, fake, ws):
    out = run_code(srv, fake, ws, {"code": "print(6 * 7)\nopen('out.txt', 'w').write('hi')"})
    assert out.startswith("退出码 0\n42")
    assert "产出的文件：" in out
    path = out.split("产出的文件：\n- ", 1)[1].splitlines()[0]
    assert "/.run/" in path and Path(path).read_text() == "hi"


def test_bash(srv, fake, ws):
    out = run_code(srv, fake, ws, {"lang": "bash", "code": "echo $((2 + 3))"})
    assert out.startswith("退出码 0\n5")


@pytest.mark.skipif(not shutil.which("node"), reason="没有 node")
def test_node(srv, fake, ws):
    out = run_code(srv, fake, ws, {"lang": "node", "code": "console.log(2 + 3)"})
    assert out.startswith("退出码 0\n5")


def test_files_and_exit_code(srv, fake, ws):
    out = run_code(srv, fake, ws, {"code": "import sys\nprint(open('data/in.txt').read())\nsys.exit(3)",
                                   "files": {"data/in.txt": "数据"}})
    assert out.startswith("退出码 3\n数据")
    bad = run_code(srv, fake, ws, {"code": "print(1)", "files": {"../x": "y"}})
    assert bad.startswith("文件名不行")


def test_cannot_read_home_or_config(srv, fake, ws):
    secrets = [str(Path.home()), str(srv.tmp / "config.yaml"), str(srv.data / "osa.db")]
    code = "import os\n" + "".join(
        # Linux 上藏起来的目录是盖了一层空的 tmpfs：能列出来但是空的，也算读不到
        f"try:\n    print('READ' if (os.listdir({p!r}) if os.path.isdir({p!r}) else open({p!r}, 'rb').read(1)) else 'DENIED')\n"
        f"except OSError:\n    print('DENIED')\n" for p in secrets)
    out = run_code(srv, fake, ws, {"code": code})
    assert out.startswith("退出码 0"), out
    assert out.count("DENIED") == len(secrets) and "READ" not in out, out


def test_timeout(srv, fake, ws):
    out = run_code(srv, fake, ws, {"code": "import time\nprint('start', flush=True)\ntime.sleep(20)", "timeout": 2})
    assert out.startswith("超时了：2 秒内没跑完，已经停掉\nstart"), out


def test_bad_lang_and_empty(srv, fake, ws):
    assert run_code(srv, fake, ws, {"lang": "ruby", "code": "puts 1"}).startswith("不支持 ruby")
    assert run_code(srv, fake, ws, {"code": "  "}) == "没有要跑的代码"


def test_quick_code_in_chat(srv, fake, ws):
    b = srv.make_bot("Calc")
    fake.script("router:Calc", Call("quick_code", {"code": "print(6 * 7)"}), "答案是 42")
    ws.say(b["chat_id"], "6 乘 7 是多少")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="答案是 42"), timeout=20, what="回答")
    bodies = fake.bodies("router", "Calc")
    names = [t["function"]["name"] for t in bodies[0]["tools"]]
    assert "quick_code" in names
    tool = [m for m in bodies[-1]["messages"] if m["role"] == "tool"]
    assert tool and tool[0]["content"].startswith("退出码 0\n42")


def test_quick_code_timeout(srv, fake, ws):
    b = srv.make_bot("Slow")
    fake.script("router:Slow", Call("quick_code", {"code": "import time\ntime.sleep(30)"}), "太慢了")
    ws.say(b["chat_id"], "算一下")
    until(lambda: srv.find(b["chat_id"], role="assistant", text="太慢了"), timeout=30, what="回答")
    tool = [m for m in fake.bodies("router", "Slow")[-1]["messages"] if m["role"] == "tool"]
    assert tool[0]["content"] == "超时了，改派后台任务"
