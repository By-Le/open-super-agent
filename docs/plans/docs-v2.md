# 资料库文档 v2：真机检查、给外面的 Agent 用、数据库

> 0.25.0 起这部分从 OSA 撤回，拆成独立项目 [Open Agent Docs](https://github.com/kadaliao/open-agent-docs)。下面提到的代码在 OSA 的 git 历史里（最后一版是 `db1352b`）。

[v1](docs.md)（按段编辑、块编辑器、评论叫醒 Bot、子页面、版本历史）2026-11-15 做完并上线（`6366a90`，CI 修复 `ad108fe`）。
这份是接着做的计划：先在真机上把 v1 过一遍，再把文档开放给 OSA 外面的 Agent（Claude Code、Codex……），最后做数据库（多维表）。

## 进度

| 步 | 内容 | 状态 |
|---|---|---|
| 1 | 真机检查（iPhone App、桌面 App）和 lsp 上的真实数据 | 完成（2026-11-15，结果见第 1 步末尾） |
| 2 | 对外的 MCP 端点：外面的 Agent 读写资料库 | 完成（2026-11-15 上线，线上验收过；结果见第 2 步末尾） |
| 3 | 数据库：带属性的表（任务看板、会议列表） | 完成（2026-11-15 上线；结果见第 3 步末尾） |
| — | 顺手可做的小事 | 按需 |

## 还没做的（新会话从这里挑）

三步和后面两批（见文末“后续两批”）都做完了，剩下这些：

- **嵌套的子项单独成段**（没做，有意留着）：现在嵌套的子项跟着上一级算一段。要改就得服务器的切法（`blocks.split` / `join` 的不变式）
  和编辑器（`markdown.ts` 按缩进重建嵌套、`blockIds` 给嵌套项编号、按项写回时的缩进、作者条）一起改，
  而且上线后所有带嵌套列表的老页面会按新切法重切一遍（编号按指纹对齐，被拆开的那几段作者会清空）。出错就是改坏用户的文档，单独开一次做、先在 lsp 的数据副本上演练。
- **App 桥接按路径**：代码做完了（`4b204f3`），要发新版桌面 App 和 iPhone App 才生效（iOS 的 `cargo check` 在本机跑不了，插件要 Xcode 的环境，发版脚本里会编）。
- **真机**（第 1 步）：模拟器上两次看到按一下回车出了两个空段（驱动点按很慢的时候），要在真机上留意。
  lsp 上的 Bot 交接还走 Notion（用户定的，别去改 Bot 的说明）。
- **数据库可以再做的**：看板按列分页（现在整个看板一起往下滚着加载）；表格真正的虚拟滚动（现在一万行全滚出来会有一万个 DOM 行）；
  公式里引用别的库（现在只能沿关联用汇总）；按钮能新建一行、打开链接（现在只能改属性、写评论）。

## 现在有什么（代码地图）

| 地方 | 做什么 |
|---|---|
| `osa/blocks.py` | 切段：Markdown 正文切成段，只切行区间不改文字；列表每项、表格每行各一段；段之间的 gap；`split(join(parts)) == parts` |
| `osa/pagedoc.py` | 按段读写：`load`（段 + 编号/作者，指纹对不上就重建）、`apply`（按段改，冲突只看要改的段）、`rewrite`（整页重写保住没变的段）、版本历史、`render`（read_page 的样子）、`edit_page` 工具 |
| `osa/pagecomments.py` | 评论：挂在段上的评论串、叫醒 Bot（`post` → `dm.wake`）、自动回帖（`after_turn` / `after_task`）、`comment_page` 工具 |
| `osa/library.py` | 页面存储、`save_page` / `read_page` 工具、子页面（`move`、`family`、删除时子页面上移）、回收站（`page_trash`）、每轮附上的页面摘要（`recent_pages`） |
| `osa/agentapi.py` | 外部 Agent：`/mcp` 端点（无状态、JSON 响应）、`agent_tokens` 表、8 个工具（复用 search / library / pagedoc / pagecomments） |
| `osa/server.py` | `/api/pages/{id}/blocks`、`/versions`、`/comments`、`/move`；事件 `page_changed`（只这一页的段变了）、`pages_changed`（列表要刷新）、`page_comments` |
| `osa/router.py`、`osa/memory.py` | `COMMENT_TURN`、批注通知的渲染（`kind=page_comment`）、`Task.comment` |
| `web/src/components/doc/` | 编辑器（懒加载）：`markdown.ts` 段 ↔ 文档、`blockIds.ts` 段编号、`sync.ts` 保存/冲突/合并、`gutter.ts` 作者条、`nodes.tsx` 提示框/折叠块/原文块/软换行、`menus.tsx`、`comments.tsx`、`history.tsx` + `diff.ts` |
| `web/src/authors.ts` | 段、评论、历史的作者名（user / bot:N / agent:N），编辑器、评论、历史、页面列表共用 |
| `web/src/components/AgentTokens.tsx` | 设置 → 外部 Agent：建 token、各家接法、撤销 |
| `web/src/components/PageTree.tsx` | 资料库的页面树、挪到哪里的对话框 |
| `osa/dbs.py` | 数据库：表结构、值的写法（工具写名字、网页写 id）、筛选排序、加行改行（按键查冲突）、视图、库的历史（表结构、视图）和撤销、`db_*` 工具、`read_page` 的样子、分页（`public` 的 limit / include） |
| `osa/dbcalc.py` | 数据库里算出来的：公式（解析、算）、汇总的算法；`dbs._Calc` 读行时算公式、汇总、双向关联的反向那一半 |
| `web/src/components/db/` | 数据库页面：`DbView`（视图标签、表格、看板）、`cells`（每种属性的格子）、`menus`（表头、筛选、排序、视图、历史）、`PropsBar`（行页面的属性栏）、`Popover` |
| `tests/test_blocks.py`、`test_pages.py`、`test_page_comments.py`、`test_agentapi.py`、`test_dbs.py` | 后端测试 |
| `tests/browser/` | 浏览器回归脚本（手动跑，见里面的 README） |

## 接手前要知道的

- **正文以 Markdown 为准，只有 `pages.content` 一份。** `block_meta` 只记编号、作者、版本、指纹，按位置对应切出来的段。
  服务器的切法是唯一标准：网页编辑器保存后按返回的段校正，别在前端另搞一套边界。
- **没改过的段一个字都不能动。** 后端只切行；前端只有节点真变了、而且写回的 Markdown 和“原节点写回的样子”不同才发。
  新加功能（比如数据库的属性）也要守这一条：Bot 写的原文不能被编辑器、序列化“顺手”改样。
- **冲突按段：** 要改的段 `v > base` 才算冲突，整批不写，把那几段现在的内容和改的人告诉调用方。
- **事件别乱发：** 用户在网页上保存只发 `page_changed`；`pages_changed` 会让列表、页面卡片整个重拉。
- **评论只有用户写的会叫醒 Bot**（Bot 写的、以后外面 Agent 写的都不叫醒），不然 Bot 之间会来回停不下来。
  `woke.from` 是那串评论的第一条，不是 Bot 编号。自动回帖从“这一轮开始”算有没有回过。
- **编辑器的坑：**
  - `useEditor` 要 `immediatelyRender: false`。否则 effect 拿到的是被 Tiptap 一毫秒后销毁的那个实例。
  - 段落里的软换行要转成带 `soft` 标记的换行节点。否则一编辑就被 ProseMirror 读成硬换行，写回成 `"  \n"`。
  - Tiptap 的 marked 实例会切出 `taskList` / `taskItem` / `orderedList` 这几种 token，加白名单时记得看全。
  - 表格按行写回、不补空格；格子里只放一段文字。
- **改表结构时**，`python -m osa.i18n` 和 `python -c "import osa.server"` 会对真的 `data/osa.db` 跑迁移，一律带临时的 `OSA_DATA_DIR` / `OSA_CONFIG`。
  多语言检查看退出码，而且要看“还有 N 行中文没包进 t()”那一行，只看缺几句不够（v1 上线时 CI 就是卡在这里）。
- **数据库：** 库是 `pages.kind = 'db'`，行是它下面 `kind = 'row'` 的子页面（不进页面树、`/api/pages` 不列、不能挪出库、页面数上限不算它）。
  行的属性改动走 `pagedoc.commit_props`（涨版本、记历史）；改表结构走 `dbs.change_schema`（不涨行的版本，记一条 `label = schema` 的库历史）。
  行的名称可以是空的（别默认“未命名页面”：就地打字会接在后面）。资料库的 `PageView` 换页时要等 `p.id === id` 再画，不然旧页面的数据配新编号会请求错接口。
- **部署：** 见 `deploy/lsp/README.md`。前端有新依赖时镜像会重新 `npm ci`（v1 那次构建约 1 分钟，没有想象的久）。

## 第 1 步：真机检查（先做，半天）

v1 只在 Chrome（Playwright）里测过，iPhone App 和桌面 App 用的是 WKWebView（Safari 内核），还没在真机上点过。

- **iPhone App（模拟器或真机）**：
  - 打开页面，用中文输入法打字，打到一半时让 Bot 改另一段，看组字会不会被打断。
  - 段首打 `、` 出斜杠菜单；长按选字出格式菜单，看会不会和系统的复制/粘贴菜单打架。
  - 从左边缘右滑返回会不会误触编辑器；键盘弹起时光标会不会被挡住。
  - 评论面板全屏打开、关掉；切到后台再回来，没存的会不会补存（`visibilitychange` / `pagehide`）。
- **桌面 App**：页面地址 `/library/pages/<id>` 刷新（Cmd+R）以后，系统通知、角标这些桥接（IPC）还能不能用。
- **lsp 上的真实数据**：
  - 老页面第一次打开时会补上段编号，看作者是否都记成了建它的 Bot。
  - 打开几个模型写的长报告，看“Markdown 原文”块多不多。多的话，看是哪种写法没认出来，补进 `markdown.ts` 的白名单或者新写节点。
  - 让 Nova 在群里交接一次真实工作。
- 发现的问题直接修，每个问题一个提交。

**结果（2026-11-15）**：iPhone 在模拟器上测（`mobile/uitest/run.sh` 新加了点键盘、长按、按名字点这些步骤），lsp 的数据拷了一份在本机测。

- 没问题的：拼音组字时别人改另一段，组字不断，改动等组完再合进来；切后台、切后台马上杀掉 App，打的字都存上了；
  长按选字时格式菜单和系统的拷贝/粘贴菜单一上一下不打架；桌面 App 整页重新载入 `/library/pages/<id>` 以后通知、角标、
  连接这台 Mac 这些桥接都能用；lsp 上 22 个老页面补的段编号作者都是建它的 Bot，没有一个“Markdown 原文”块，打开不会改页面。
- 修了的：页面底下的“＋ 子页面”点不动（被编辑器的下边距盖住，桌面也有，`93ee702`）；手机上历史、评论面板的标题栏钻到状态栏底下关不掉，
  点开一版时对比被整屏的列表盖住（`2b9ad13`）；键盘弹起时斜杠菜单落到键盘后面、格式菜单离选区太近（`cfcd5b1`）。
- 没做的：lsp 上的 Bot（除了 Nova）的说明里写着“产出一律写进 Notion，save_page 停用”，真实交接走的是 Notion，没在 lsp 上让 Nova 交接；
  要不要改成用资料库由用户定。交接本身 v1 用真模型测过（`tests/browser/handoff_live.py`）。
- 留意的：模拟器上两次看到按一下回车出了两个空段，都是驱动点“换行”键特别慢（7–9 秒、重试过）的时候，正常点了四次都没有。
  可能是 ProseMirror 在 iOS 上处理回车的 200 毫秒兜底在主线程忙时多切了一次，要在真机上留意。

## 第 2 步：对外的 MCP 端点

**为什么**：用户最初想要的是“给 Agent 用的文档系统”。现在只有 OSA 里的 Bot 能写资料库，接上 MCP 以后，Claude Code、Codex 也能把产出写进来，
和 OSA 的 Bot 在同一份文档上接着改，人在网页上看、批注。

**设计**

- **挂载**：
  - `mcp` 2.3.0 里 FastMCP 改名叫 `MCPServer`（`from mcp.server.mcpserver import MCPServer`），有 `streamable_http_app()`，可以挂在 FastAPI 的 `/mcp` 下。
  - 它的 `session_manager` 要在应用的生命周期里跑起来（放进 `server.py` 现有的 lifespan）。
  - 新模块 `osa/agentapi.py`。
- **鉴权**：
  - 新表 `agent_tokens(id, name, hash, scope, created, last_used, revoked)`。`scope` 是 `read` 或 `write`；token 只在创建时显示一次，库里只存 sha256。
  - `auth.py` 的中间件对 `/mcp` 不看登录 cookie，只认 `Authorization: Bearer`，同时照样检查 Host。
  - 演示服务器（`security.demo`）上整个关掉。
  - 设置里加一页“外部 Agent”：新建（填名字、选只读还是可写）、列表（最后用过的时间）、撤销。
- **身份**：作者记成 `agent:<id>`。下面几处要认这种作者：
  - `pagedoc.name_of`；
  - 前端 `DocEditor` 的 `nameOf` 和 `gutter` 的 `who`、`comments.tsx` 的 `name`；
  - 名字放进 bootstrap 的 `agents` 列表。
  - 外面 Agent 的评论不叫醒 Bot（走 `pagecomments.add`，不走 `post`）。锁定的页面它只能读。
- **工具**（都复用现有函数，别另写一套）：
  - `search_pages(query)` → `search.search(kinds=("page",))`
  - `list_pages(parent_id?)` → 标题树（`library.list_pages` + `family`）
  - `read_page(page_id, start?)` → `pagedoc.render`
  - `create_page(title, content, parent_id?)` → `library.save_page(None, …, by="agent:N", parent_id=…)`
  - `edit_page(page_id, version, ops)` → `pagedoc.apply`，ops 写法和 Bot 的 `edit_page` 一样（`_bot_ops`）
  - `rewrite_page(page_id, version, content)` → `library.save_page(…, base=version)`
  - `list_comments(page_id)` / `comment_page(…)` → `pagecomments`
  - 写操作照样 `registry.emit("page_changed" / "pages_changed" / "page_comments")`，网页上实时看到。
- **工具说明**要写清楚：先 `read_page` 拿版本号和段落编号；冲突时会告诉你现在的内容，按新版本重提。

**测试**

- `tests/test_agentapi.py`：
  - token 的建、查、撤销；
  - 没 token、错 token、只读 token 去写，都被拒；
  - 用 MCP 客户端（或者直接发 JSON-RPC，经 `httpx.ASGITransport`）调一遍每个工具；
  - 作者记成 `agent:N`，事件发出去了，锁定页面不能写。
- 浏览器：照 `tests/browser/bot_edit_live.py` 写一个，用 MCP 客户端改页面，看开着的编辑器实时出现、作者条显示 token 的名字。

**验收**：在 Mac 上执行

```sh
claude mcp add --transport http osa https://osa.cclsp.top/mcp --header "Authorization: Bearer <token>"
```

让 Claude Code 读一个页面、改一段、在某段写评论，网页上实时看到；撤销 token 以后再调就被拒。

**文档**：`docs/configuration.md` 加“让外面的 Agent 读写资料库”一节（Claude Code、Codex 怎么接），CHANGELOG、features 中英两份。

**结果（2026-11-15）**：

- 做法和设计稿的出入：
  - 会话管理每个请求起一个、处理完就收（无状态模式本来每个请求就独立），不进 lifespan：常驻的任务会让测试的 `settle()` 每次等满 5 秒。
  - `MCPServer` 建的时候会 `logging.basicConfig(INFO)`，建完把根日志器还原，不然所有库（httpx 会记下每个出站地址）的 INFO 都打出来。
  - 工具结果只给文字（`structured_output=False`），不然同样的内容在 `structuredContent` 里再发一遍。
  - 只读的 token 在 `tools/list` 里看不到写的工具（重写了 `_handle_list_tools`，私有方法，升级 mcp 时 `test_read_only` 会抓到）。
  - 页面多了 `agent_id`（外部 Agent 建的，`bot_id` 是空的），`read_page` 顶上、资料库列表写着是它建的。
  - `/mcp` 的 401 是 JSON；SDK 客户端只报“Server returned an error response”，看不到状态码。
- 测过：`tests/test_agentapi.py`（11 个，SDK 客户端经 ASGI 调全部工具）；浏览器 `tests/browser/agent_live.py` 全过；
  真的 Claude Code（`claude -p --mcp-config`）读页面、改一段、在某段写评论，只动了那一段、作者是 `agent:N`、没叫醒 Bot；
  真的 Codex（`url` + `bearer_token_env_var`）能读。
- 顺手修的：页面最后自动补的空段在合并别人的改动时没留在最后，Bot / 外部 Agent 往列表末尾加的项被它隔成另一个列表（只在开着的编辑器里，刷新就好）。
- 上线（`3440557`，2026-11-15 18:28）：Caddy 整站转发，不用改。真的 Claude Code 接 `https://osa.cclsp.top/mcp` 读页面、改一段、在某段写评论，只动了那一段、作者是 `agent:1`、没叫醒 Bot；撤销后 401；演示站 `/mcp` 是 404。验收用的 token「Claude Code（验收）」撤销了留在列表里，测试页删了。

## 第 3 步：数据库（带属性的表）

**为什么**：用户的 Bot 在 Notion 里也用数据库（应用市场里 Notion 的说明就是“往数据库里加条目、改属性”），
比如任务看板、会议列表。没有它，交接时的“状态表”还得回 Notion。

**用户定了（2026-11-15）**：属性类型参考 Notion、由我定；要看板；Bot 能改表结构。

**设计**

- **属性类型**（参考 Notion）：
  - 存值的：名称（`title`，就是行页面的标题，每个库一个、不能删）、文本、数字（显示格式：数字 / 千分位 / 百分比 / 人民币 / 美元）、
    单选、多选、状态（选项分“未开始 / 进行中 / 已完成”三组，默认三个选项）、日期（`{start, end?}`，`2026-11-15` 或 `2026-11-15 14:00`）、
    人员（多个：`user` / `bot:N` / `agent:N`）、勾选、网址、邮箱、电话。
  - 自动算的（只读）：创建时间、最后编辑时间、创建人、最后编辑人。
  - 先不做：公式、关联、汇总、文件、自动编号、按钮。
  - 选项 `{id, name, color}`，值里存选项 id（改名不用改每一行），工具进出都用名字。颜色用 Notion 那 10 种。
- **数据库是一个页面**：`pages.kind`（新列：`page` / `db` / `row`，默认 `page`）。
  - 表结构 `pages.schema`：`[{key, name, type, options?, format?}]`，第一项是名称。
  - 视图 `pages.views`：`[{id, name, type: table | board, sort, filter, group_by?, hidden, widths}]`。新建的库带“表格”和“看板”（按状态分列）两个视图。
  - 改表结构：加、改名、改类型（能转的转过去，转不了的清空）、改选项、删。按属性后写的为准，不算冲突。
  - 表结构也记历史（用户 2026-11-15 要求：Bot 能改结构，删列、改类型会清掉各行的值，要撤得回来）：每次改动记一条
    （`page_versions`，`label = schema`：改到的属性改前改后的定义、被清掉或转换的格子的旧值和新值）。库页面上的“历史”能撤销任意一条：
    只动那次改到的属性，删掉的列回到原来的位置、值放回去；之后又被人改过的格子留着现在的值。撤销也记一条，能再撤回来。
- **每一行是它的子页面**（`kind = 'row'`，`parent_id` 指向库，`pos` 是手动排的顺序）：
  - 属性存 `pages.props`，正文还是 Markdown，按段编辑、评论、记历史都照旧。行不进页面树、不能挪出库、下面不放子页面。
  - 属性改动进版本历史（`page_versions.props`）；回到某一版连属性一起回去。
  - 冲突按键：`props_meta` 记每个键最后改动的版本和人，给了版本号时只有要改的键被别人先改过才冲突。
  - 每个库最多 1000 行；页面总数 1000 的上限不算行。删库连行一起删。锁定的库（或行）Bot 只能读。
- **筛选、排序在服务器上做**（工具和网页用同一套）：筛选条件都要满足；排序可以按几列。
- **Bot 工具**（结果写成紧凑的表格文字）：`db_create`（建库，可以带属性）、`db_schema`（改表结构）、`db_query`（筛选、排序、条数）、
  `db_add`（加行，属性用名字写）、`db_update`（改属性，带版本号时按键查冲突）。行的正文用 `edit_page` 改；`read_page` 读库给结构和前面的行。
  外部 Agent 的 MCP 也有这几个。Bot 不删行（和页面一样，删由用户来）。
- **网页**：
  - 库页面：视图标签（表格 / 看板，可以加）、筛选、排序、显示哪些列、新建；表格每格点了就改；表头菜单改名、改类型、改选项、隐藏、删列；“＋”加列。
  - 看板：按单选或状态分列（加一列“没有”），卡片上显示标题和几个属性，拖动换列、调顺序，每列底下能新建。
  - 点行打开成页面：v1 的编辑器，顶上多一块属性栏，路径上能回到库。
  - 改了发 `db_changed`，开着的库重新取一遍（1000 行以内，不做虚拟滚动）。
- **搜索**：行的属性文字拼进索引。**分享**：分享的库显示成只读的表格（第一个视图的筛选和排序）。
- **测试**：`tests/test_dbs.py`；浏览器脚本 `tests/browser/database.py`。

**结果（2026-11-15）**：

- 代码：`osa/dbs.py`（表结构、值、筛选排序、行、表结构历史和撤销、工具），`web/src/components/db/`（`DbView` 表格和看板、`cells` 格子、`menus` 菜单和历史、
  `PropsBar` 行页面的属性栏、`Popover`）。接口 `/api/dbs/…`，事件 `db_changed`（前端转成 `osa-db`）。
- 测过：`tests/test_dbs.py`（11 个）、`test_agentapi.py` 加了数据库工具；浏览器 `database.py` 20 项，`editor` / `editor_more` / `tree` / `history` / `agent_live` 都过。
- 中途修的：行的标题默认不再是“未命名页面”（新建一行时就地打的字会接在它后面）；从库点进一行（或者返回）时，页面先拿旧页面的数据配新的编号，
  去请求了不存在的库 / 行（404），现在等对上了再显示。
- 上线（`08f64d0`，2026-11-15 19:41）：线上建库、加行、改属性、删列、从历史撤销、删库都走过一遍，测试库删了。
- 没做的：一个库超过 1000 行要分页、虚拟滚动；公式、关联、汇总、文件、自动编号。

**后续（2026-11-15，上线后接着做的）**：

- 行的历史：版本接口给行多带 `db_id` 和 `stale`（那一版的值放到现在的表结构下认不出的键：列删了、类型改了、选项删了，判断用 `dbs.fits`，
  拿 `normalize(create=False)` 走一遍看是不是原样）。网页 `db/PropsDiff.tsx` 按库现在的表结构画“旧值 → 新值”，`stale` 的不比、只提一句，
  每个属性能单独恢复（走 `PATCH /api/dbs/…/rows/…`，记成新的一版）。表结构变了（`osa-db` 带 `schema`）开着的那一版重新取。
  顺手修了：回到某一版（`dbs.restore_props`）原来把那一版的属性整个写回去，删掉的列、改了类型认不出的旧值也会带回来；现在只动现在表结构里有、认得出的。
- 看板：卡片右上角“⋯”（手机上一直显示，桌面悬停显示）：挪到哪一列、往上挪、往下挪（视图有排序时不显示后两个）。拖动和菜单走同一个 `place`。
- 外部 Agent：`agent_tokens.hidden`，`POST /api/agent-tokens/{id}/hide` 只对撤销了的有效；`tokens()` 不列，`names()` 照样给（作者名要用）。
  设置里撤销了的收在“已撤销的（N）”里。
- 测过：`test_dbs.py` 加 `test_restore_after_schema_change`、`test_agentapi.py` 的设置接口加删除；浏览器新脚本 `db_more.py` 20 项，
  `database` / `history` / `agent_live`（撤销了的现在要先展开才看得到，脚本改了）都过。

## 顺手可做的小事

- **段的上移、下移、删除菜单**（手机上需要）。拖拽手柄：先查 `@tiptap/extension-drag-handle` 会不会拉进 yjs。
- **交接时每轮附上页面摘要**：每轮状态里列出最近提到的至多 3 个页面，一行一个，写版本号、未处理评论数、最后改的人，每个约 30 个 token。
- **评论框里 @ 的自动补全**：复用 `Composer.tsx` 的 `.mention-list`。
- **Bot 在评论里回复时推送到手机**：频道和推送都没接这类消息。
- **回收站**：现在删页面是真删，版本历史也跟着没了。可以先改成软删除，30 天后再清。
- **嵌套的子项单独成段**：现在嵌套子项跟着上一级算一段。
- **含 HTML 的表格**：整张表放进一个原文块，改过以后每行的编号会重新生成。
- **图片**：现在只能填网址，可以接上传（复用 `media` 的上传）。
- **桌面 / 手机 App 的桥接只认域名、不认路径**：Tauri 在 macOS / iOS 上按请求的 Origin 头（没有路径）对授权，
  `UI_PATHS` 限定的“只有网页界面能调”实际没起作用，同一个域名下没被沙箱隔离的页面（比如 markdown 页面的 `/api/pages/<id>/raw`）也能调
  `openExternal`、`link` 这些命令。Bot 的报告页是沙箱隔离的（来源是 null），调不到；同域名的页面本来也能用你的登录调服务器的接口，
  所以影响有限。要补就在每个 `desktop_*` / 手机的命令里自己看 `webview.url()` 的路径，发新版 App 才生效。

## 怎么验收（每一步都要）

- 后端：`uv run pytest -q`。
- 多语言：`OSA_DATA_DIR=<临时目录> OSA_CONFIG=<临时目录>/config.yaml uv run python -m osa.i18n; echo $?`。退出码要是 0，“没包进 t()”要是 0 行。
- 前端：`cd web && npm run build`（`tsc --noEmit` 加构建）。
- 浏览器：数据副本上起服务，跑 `tests/browser/` 里相关的脚本（见 README）。新功能照着加脚本。测完停掉服务。
- 上线：`deploy/lsp/README.md`。换版前存数据库快照、确认没有正在跑的任务。换版后检查容器 healthy、应用都连上、`problem` 为空、新的前端包。

## 后续两批（2026-11-15）

**第一批**（`87b9084`…`ad91252`，已上线 21:14）：行页面的历史看得到属性的差别、能只恢复一个属性；看板卡片的“⋯”换列、上下挪；撤销了的外部 Agent 能从列表删掉。
细节见第 3 步末尾的“后续”。

**第二批**（`12e1b10`、`4b204f3`、`5e6dd8f`）：

- 数据库的新属性类型：
  - 公式（`osa/dbcalc.py`：自己写的小解析器，写法参考 Notion）。存的是带属性名的原文，改名时 `_rename_refs` 跟着改；
    互相引用成环在改表结构时就报错（`_formula_cycle`），算的时候再按栈防一次。
  - 关联存对方库的行号列表；双向的在对方库加一个 `mirror_of` 属性，值不存、读的时候从这边反查（`_backlinks`），在那边改改的是这边的行（`_set_mirror`）。
    删了这边的关联，对方那一半也去掉（`_drop_pair`）。
  - 汇总沿关联取对方行，对方的行带着算好的公式（`rows_of` 按层数递归，最多 `MAX_CHAIN` 层）。
  - 自动编号存数、表结构里记 `next`；文件存 `{name, url}`，`/api/media?file=1` 收任何文件（不是图片、视频、PDF 的只能下载），`media.used` 认属性和回收站里的引用。
  - 按钮：`POST /api/dbs/…/rows/…/press/<key>`，改属性走 `update_row`，评论走 `pagecomments.post`（叫醒 Bot）。
  - 关联着的库一起发 `db_changed`（`related_dbs`）。
- 大库：`MAX_ROWS = 10000`；网页一次要 200 行（`limit`），滚到底再要；刚加的行用 `include` 带上；看板的列数服务器数（`groups`）。
- 视图改动进库的历史（`set_views` 记一条 `kind=views`，同一个人 10 分钟内并成一条）；撤销按视图换回（`_undo_views`）。历史留 200 条。
- 手机看板长按拖：原生的 touch 事件（非 passive，拖的时候不滚页面），按横向位置找列（列只有卡片那么高），自动滚动时也重算落点。
- 回收站：删的时候把页面行、版本、评论、小应用数据整个打包进 `page_trash`，`pages` 里照旧删掉（别的查询都不用加“没删”的条件）；
  放回去时编号被占了就换新的，评论的 parent 跟着换；原来的上级不在了放到最上层。`media.gc_loop` 每天先清 30 天前的。
- 编辑器：传图片（`insertImages`，和对话里一样先缩）、粘贴和拖进来的图片；评论框的 @ 补全；手机上的段菜单（`BlockHandle`，挪的是节点本身，段编号跟着走）。
- 原文块整组改：`replace` 带 `group`，服务器按内容对齐（`blocks.align`）保住编号。顺手修了 `sync.saved`：发出去的段服务器原样留着时（不在 blocks 里）也算存上了，
  不然原文块的第一行没变时，它一直算没存，离开或刷新时整张表又被追加一遍（老的“删掉其余的段”做法也有这个问题）。
- Bot 回复了用户起头的评论：`page_comments` 事件带 `reply`，没开着网页时 `push.message_for` 推到手机。
- 每轮状态附上对话最近 30 条消息里提到的页面（链接和评论通知里的页面编号），至多 3 个。
- 测过：`uv run pytest`（新加 test_formula / relation_rollup / files_unique_id_button / press_and_related_events / paging / view_history / trash / recent_pages / replace_group_keeps_ids，
  推送的断言加在评论的测试里）；浏览器 `db_types.py` 20 项、`docs_more.py` 11 项，老的 `database` / `db_more` / `editor` / `editor_more` / `history` / `tree` / `agent_live` 都过；
  桌面 `cargo test`（`ui_paths`）、手机 `cargo check`（本机）。

