"""两个版本发给模型的请求逐字一样：同一个剧本在 Python 版和 Rust 版上各跑一遍，比对每次请求的 messages、tools 和参数。

提示词、工具 schema、上下文的拼法差一个字，模型服务的前缀缓存就不命中，Bot 的行为也可能变。只在 Rust 版构建好时跑。
比之前把会变的东西（编号、时间、流式 id）换成占位符。
"""
import os
import re
from pathlib import Path

import pytest

from conftest import RUST_BIN, Server, Socket, idle, until
from fakellm_http import Call, FakeLLM

pytestmark = pytest.mark.skipif(not Path(RUST_BIN).exists(), reason="没有构建 Rust 版")


def normalize(v):
    if isinstance(v, dict):
        return {k: normalize(x) for k, x in v.items()}
    if isinstance(v, list):
        return [normalize(x) for x in v]
    if isinstance(v, str):
        v = re.sub(r"#\d+", "#N", v)
        v = re.sub(r"\d{4}-\d{2}-\d{2}", "DATE", v)
        v = re.sub(r"\d{2}-\d{2} 任务", "MM-DD 任务", v)
        return v
    return v


def run(impl: str, scenario) -> list[tuple[str, str, dict]]:
    llm = FakeLLM()
    old = os.environ.get("OSA_IMPL")
    os.environ["OSA_IMPL"] = impl
    import conftest
    conftest.IMPL = impl
    try:
        srv = Server(llm)
        ws = Socket(srv)
        try:
            scenario(srv, llm, ws)
            idle(srv)
        finally:
            ws.close()
            srv.stop()
    finally:
        conftest.IMPL = old or "python"
        if old is None:
            os.environ.pop("OSA_IMPL", None)
        else:
            os.environ["OSA_IMPL"] = old
    return [(r, n, normalize(b)) for r, n, b in llm.requests]


def compare(scenario):
    a, b = run("python", scenario), run("rust", scenario)
    assert [(r, n) for r, n, _ in a] == [(r, n) for r, n, _ in b], "请求的顺序不一样"
    diffs = []
    for i, ((role, name, x), (_, _, y)) in enumerate(zip(a, b)):
        for k in sorted(set(x) | set(y)):
            if x.get(k) != y.get(k):
                xs, ys = str(x.get(k)), str(y.get(k))
                j = next((n for n, (p, q) in enumerate(zip(xs, ys)) if p != q), min(len(xs), len(ys)))
                diffs.append(f"第 {i + 1} 次请求（{role} {name}）的 {k} 不一样，从第 {j} 个字开始：\n"
                             f"python: …{xs[max(0, j - 60):j + 160]}\nrust:   …{ys[max(0, j - 60):j + 160]}")
    if diffs:
        pytest.fail("\n\n".join(diffs))


def test_direct_chat_with_task():
    def scenario(srv, llm, ws):
        until(lambda: any(x["name"] == "demo" and x.get("state") == "connected" for x in srv.get("/api/apps")["apps"]),
              timeout=30, what="假应用连上")
        a = srv.make_bot("Parity", "负责比对")
        srv.post(f"/api/bots/{a['id']}/memories", {"text": "用户喜欢简短的回答"})
        llm.script("router:Parity", Call("dispatch", {"goal": "查一下天气"}), "派出去了", "收到结果")
        llm.script("worker", [Call("demo__search_notes", {"query": "天气"}), Call("recall", {"id": 1})], "晴")
        ws.say(a["chat_id"], "查下天气")
        until(lambda: srv.find(a["chat_id"], kind="result"), what="任务结果")
        ws.say(a["chat_id"], "谢谢")
        until(lambda: len(srv.find(a["chat_id"], role="assistant")) >= 2, what="第二轮")
    compare(scenario)


def test_group_and_dm():
    def scenario(srv, llm, ws):
        until(lambda: any(x["name"] == "demo" and x.get("state") == "connected" for x in srv.get("/api/apps")["apps"]),
              timeout=30, what="假应用连上")
        a, b, c = srv.make_bot("Lead"), srv.make_bot("Helper"), srv.make_bot("Outside")
        g = srv.post("/api/groups", {"bot_ids": [a["id"], b["id"]], "lead": a["id"]})
        llm.script("router:Lead", Call("handoff", {"to": "Helper", "request": "你来答"}), "交给 Helper 了")
        llm.script("router:Helper", Call("message_bot", {"to": "Outside", "text": "帮我查个数"}), "等 Outside 回信", "Outside 说是 42")
        llm.script("router:Outside", "42")
        ws.say(g["id"], "大家好，帮个忙")
        until(lambda: srv.find(g["id"], role="assistant", text="Outside 说是 42"), what="回信后的发言")
    compare(scenario)
