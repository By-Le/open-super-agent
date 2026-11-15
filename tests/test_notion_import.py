"""从 Notion 导入（osa/notion_import.py）。"""
import httpx
from osa import mcp_hub, notion_import, server


def http():
    return httpx.AsyncClient(transport=httpx.ASGITransport(app=server.app), base_url="http://127.0.0.1")


class FakeResult:
    def __init__(self, text, error=False):
        self.content = [type("C", (), {"text": text})()]
        self.is_error = error


class FakeSession:
    def __init__(self):
        self.calls = []

    async def call_tool(self, tool, args):
        self.calls.append((tool, args))
        return FakeResult("坏了", True) if args.get("bad") else FakeResult(f"{tool}:{args}")


async def test_read_only_calls(monkeypatch):
    fake = FakeSession()
    async with http() as c:
        monkeypatch.setattr(mcp_hub, "_sessions", {})
        assert (await c.post("/api/notion/read", json={"tool": "notion-search", "args": {"query": "x"}})).status_code == 409
        monkeypatch.setattr(mcp_hub, "_sessions", {"notion": fake})
        r = await c.post("/api/notion/read", json={"tool": "notion-search", "args": {"query": "公司"}})
        assert r.status_code == 200 and r.json()["text"].startswith("notion-search")
        assert (await c.post("/api/notion/read", json={"tool": "notion-update-page", "args": {}})).status_code == 400   # 不让写
        assert (await c.post("/api/notion/read", json={"tool": "notion-fetch", "args": {"bad": 1}})).status_code == 502
    assert [t for t, _ in fake.calls] == ["notion-search", "notion-fetch"]
