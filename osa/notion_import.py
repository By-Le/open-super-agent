"""从 Notion 导入资料库：用应用市场里连着的 Notion（官方托管的 MCP，用户已经登录授权）读页面和数据库，
在资料库里建成同样层级的页面和数据库。

只调读的工具（READ_TOOLS），不往 Notion 写。用服务器进程里现成的会话（mcp_hub.session_of），
不另起连接：另起一个会刷新 OAuth 令牌，服务器手里的那个就失效了。
"""
import asyncio

from . import i18n, mcp_hub

APP = "notion"
READ_TOOLS = {"notion-search", "notion-fetch", "notion-query-data-sources", "notion-get-users", "notion-get-self",
              "notion-get-comments", "notion-list-shared-pages", "notion-list-recent-pages"}
TIMEOUT = 60


class NotConnected(RuntimeError):
    pass


async def call(tool: str, args: dict, timeout: float = TIMEOUT) -> str:
    """调一个只读的 Notion 工具，返回它给的文字。报错抛 RuntimeError（说清楚是 Notion 那边的）。"""
    if tool not in READ_TOOLS:
        raise ValueError(i18n.t("只能用 Notion 读的工具"))
    session = mcp_hub.session_of(APP)
    if session is None:
        raise NotConnected(i18n.t("Notion 没连上：先到应用市场连上 Notion"))
    res = await asyncio.wait_for(session.call_tool(tool, args), timeout)
    text = "\n".join(t for c in res.content if (t := getattr(c, "text", None)))
    if getattr(res, "is_error", None) or getattr(res, "isError", None):
        raise RuntimeError(i18n.t("Notion 报错：{error}", error=text[:300]))
    return text
