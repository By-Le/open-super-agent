"""接口测试用的 stdio 应用：一个只读工具、一个写工具（要确认）。"""
from mcp.server.mcpserver import MCPServer

app = MCPServer("demo")


@app.tool()
def search_notes(query: str = "") -> str:
    """按关键词查笔记"""
    return f"找到 1 条笔记：{query} 相关"


@app.tool()
def create_note(text: str = "") -> str:
    """新建笔记"""
    return f"已创建笔记：{text}"


if __name__ == "__main__":
    app.run("stdio")
