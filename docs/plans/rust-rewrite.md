# 服务端用 Rust 重写

2026-11-19 定的：服务端从 Python（FastAPI）整个换成 Rust，在 `rust` 分支上做（worktree 在 `../open-super-agent-rust`）。
用户自己维护，main 不冻结；main 上有了新改动，这边按需跟上。

## 为什么、要得到什么

- 内存：lsp 上主服务约 164 MB（RSS 64 + swap 100），Rust 版目标 30–50 MB。
- 启动：Python 版 import 约 0.66 秒、ready 约 1.25 秒；Rust 版目标 0.1 秒以内，桌面 App 本机模式打开更快。
- 桌面 App：最后把服务作为库链进 Tauri 外壳，一个进程；不再带 105 MB 的 Python 运行环境（run_code 要的 Python、uvx 用到时再下载）。
- 和 [Open Agent Docs](https://github.com/kadaliao/open-agent-docs) 同一套技术栈（axum 0.8、tokio、rusqlite、rmcp、reqwest），
  资料库以后可以直接把 OAD 的 crate 挂进来（见 OAD 的 `examples/rust-mount`）。

不指望它解决的：任务里的等待（模型、第三方接口、网页打开）、第三方应用进程的内存、打开 App 时的白屏
（白屏是前端 1.3 MB 的包没压缩、没缓存头，见文末“顺手修的”）。

### 实测（2026-11-19，第 1 步做完时，同一份新建的数据，本机 release 构建）

| | Python 版 | Rust 版 |
|---|---|---|
| 启动到能接请求 | 0.9–1.8 秒 | 0.05 秒 |
| 常驻内存（刚启动） | 106 MB | 20 MB |
| 程序 | 要带 Python 运行环境（桌面 App 里 105 MB） | 一个 20 MB 的文件（另有 osa-relay） |

只迁了核心部分时是 11 MB、12 MB 的程序；模块都进来以后是上面的数。

## 原则：对外一个字不改

- **接口、WebSocket 消息、SQLite 表结构、config.yaml、数据目录的布局都和 Python 版一样。** 网页、桌面 App、手机 App 不用改；
  同一个数据目录能在两个版本之间来回切，出了问题随时退回 Python 版。
- **给模型的提示词逐字照搬**（包括中文标点、换行）。提示词是产品的一部分，改它要单独决定，不在重写里顺手改。
- **用户看到的文字照旧走 i18n**：中文原文是键，`osa/locales/en.json` 编进二进制。
- **行为以 Python 版为准**。读不懂、觉得是 bug 的地方先照搬，记进本文“发现的问题”，单独决定改不改。

## 怎么验证：接口测试（tests/contract）

新写的一套黑盒测试，只通过 HTTP、WebSocket 和数据库文件和服务打交道，同一套测试跑两个版本：

```
uv run pytest tests/contract                 # 跑 Python 版
OSA_IMPL=rust uv run pytest tests/contract   # 跑 Rust 版（先 cargo build）
```

- 假模型是一个真的 HTTP 服务（`tests/contract/fakellm_http.py`），按剧本流式返回文字或工具调用，和 `tests/fakellm.py` 一样按系统提示词认角色。
- 假应用是一个 stdio 的 MCP 服务（`tests/contract/fake_mcp.py`），测应用的工具、确认卡片。
- 每个测试模块起一个服务进程，临时数据目录、临时 config.yaml。
- `contract/test_parity.py`：同一个剧本在两个版本上各跑一遍，比对每次发给模型的请求（messages、tools、参数）逐字一样。
  2026-11-19 第一次跑：除了还没迁的 quick_code / run_code，系统提示词、上下文、群聊和私信的轮次说明、记忆都一字不差。
- 一个模块迁完的标准：它的接口测试在两个版本上都通过。原来的 `tests/`（进程内的单元测试）留着测 Python 版，Rust 版的单元测试写在各自的模块里。

## 代码结构

```
server/                     Rust 工作区（独立的，不和 desktop/、mobile/ 的 Cargo 工作区混）
  Cargo.toml
  crates/osa/               服务本体（lib + bin osa-server）
    src/
      main.rs               命令行：--host --port，和 uvicorn 的用法对上
      lib.rs                模块清单
      config.rs  memory.rs  bots.rs  router.rs  worker.rs ……   和 osa/*.py 一一对应，名字不变
```

Python 到 Rust 的对应：

| Python | Rust |
|---|---|
| 模块里的全局变量（TASKS、RUNTIMES、pending……） | 模块里的 `static`（`LazyLock<Mutex<…>>`），名字不变 |
| `memory.conn`（一个 sqlite3 连接，check_same_thread=False） | 一个 `rusqlite::Connection` 放在 `Mutex` 里（`memory::db()`），语义一样：同一时间一条语句 |
| dict / list（接口、事件、meta） | `serde_json::Value`；结构固定、用得多的（Task、Bot）写成 struct |
| `asyncio.create_task` | `tokio::spawn` |
| 任务取消（`handle.cancel()` 抛 CancelledError，except 里收尾） | `CancellationToken` + `select!`，取消后照样走收尾的那段 |
| `ContextVar`（WHO、TASK_PC、CLAIM） | `tokio::task_local!` |
| `notify = broadcast` 这类回调 | `server::broadcast` 直接调（Rust 里没有循环导入的问题） |
| openai SDK 流式调用 | reqwest 自己解析 SSE（`providers::stream`），重试规则照搬 SDK：5 次、0.5–8 秒退避 |
| mcp SDK | rmcp |

## 进度

| 步 | 内容 | 状态 |
|---|---|---|
| 0 | 计划、接口测试的框架（先在 Python 版上跑通） | 完成（2026-11-19） |
| 1 | 核心：数据库和迁移、设置、登录、Bot / 对话 / 消息 / 讨论串 / 未读、记忆、调模型（流式）、Router、Worker、任务和任务过程、确认、WebSocket、应用（MCP）、沙箱、内置工具、技能 | 完成（2026-11-19）：接口测试 Python 87 / Rust 85 全过（parity 只在 Python 那边跑一次），发给模型的请求逐字一样；现在的网页在 Rust 版上能聊天、派任务、看过程、应用市场、观测、设置，没有报错的接口 |
| 2 | 电脑：Instabox、Docker、本机 Chrome、派任务到 Mac | A 完成（2026-11-20）：共用部分、电脑工具、Docker、Instabox、电脑和文件的接口、VNC 转发，别的模块里的桩都接上了；B（本机 Chrome、整台 Mac、派任务到 Mac）未开始 |
| 3 | 外围：事件、定时任务、消息来源、聊天软件、画图和视频、资料库页面、小应用、备份、推送、中转 | 大部分跟第 1 步一起做完了（分模块并行），剩下的见下面“还没迁的” |
| 4 | 桌面 App 把服务链进 Tauri 外壳；Python / uv 用到时再下载；Docker 镜像换成 Rust 版 | 未开始 |

第 1 步做完停下来给用户看：用 Rust 版打开现在的网页，能和 Bot 聊天、能跑带工具的任务，接口测试在两个版本上都通过。

## 还没迁的（2026-11-19）

- **电脑（第 2 步 B）**：本机 Chrome（host）、整台 Mac（macdesk）、派任务到你的 Mac（mac / link / agent，/api/computer/links、
  /api/computer/macs/{id}…、/api/computer/link）。现在选了 host 会照实说“Rust 版还没有本机电脑”；macs 列表是空的。
- **事件来源**：收邮件（IMAP）、日历、天气、地震、价格、文件夹。列表里照样有，选了提示“Rust 版还没有这种事件来源”。
- **没在真环境里跑过的**：Linux 上的沙箱（bwrap / unshare）、飞书 / 钉钉 / 企业微信 / Discord（只编译过）、推送中转连真的 APNs、
  imap 检查、OAuth 连真的服务（假服务上整套流程测过）。上 lsp 之前要在 Linux 上把接口测试跑一遍。

## 电脑的结构（第 2 步 A 定下的，B 照着接）

- `computer/base.rs`：`Computer` 是所有电脑共用的部分（谁在用 users、排队 waiting、演示独占 exclusive、watch、对话用的窗口、
  asking、接手 / 交还、浏览器操作 browser()、page_shot、front），`Backend` trait 是每种电脑自己的部分。
- Backend 必须实现：kind、status、start、pause、exec、open_vnc。带默认实现（走终端命令）、本机电脑要重写的：
  read_bytes / read_range / write_bytes / ls / stat_paths / rm / upload_paths、helper_run（本机在本机跑 cdp_helper.py）、
  look / screenshot / type_text / press / shell / peek，以及 on_takeover / on_handback / on_release（Python 的 release 重写）。
- HOSTS 那一组（B 实现）：shell_enabled、desktop_enabled、desk_supported、desk_busy、desk_state、show、grant、
  desk(op, key, args)（screenshot / inspect / focused / click / drag / type / key / scroll / open_app，返回的 JSON 里截图放在
  "png"，base64）、desk_claim / desk_release / desk_allowed / desk_ok / desk_denied / set_desk_denied，mac 用的 lid、name。
- 派到 Mac 的任务：router 跑任务时包了 `computer::with_task_pc`，B 在 use_mac 里调 `computer::set_task_pc(那台 Mac)`，
  `computer::current()` 就是它。pick_mac / use_mac / link_rows / macs_state 现在是 mod.rs 里的占位。
- 电脑里运行的三个脚本不改：cdp_helper.py（字节原样写进电脑，路径 /tmp/osa-cdp-<sha1 前 8 位>.py）、computerd.py、镜像。
- 接口测试 contract/test_computer.py 用真的 osa-computer:2 容器加测试里的假 computerd（和 lsp 上 compose 的电脑容器同一条路），
  没装 Docker、没有镜像就跳过；Instabox 用假 API（INSTABOX_API_URL）。这台 Mac 的 shell 设了 DOCKER_DEFAULT_PLATFORM，
  起容器要带 --platform（local.rs 的 platform() 也是为这个）。

## 迁一个模块的做法

- **一个模块一个文件**（osa/foo.py → server/crates/osa/src/foo.rs），接口写在这个模块自己的 `pub fn routes() -> axum::Router` 里，
  `routes.rs` 已经把所有模块的 routes() 合起来了。别的模块的文件不改；要别人的函数就调它，它还是桩（stub）就先在自己这边绕开、记下来。
- 共用的文件只做追加：`lib.rs`（`pub mod`、`modules_migrate()` 里加建表、`background()` 里加后台循环）、`Cargo.toml`（依赖）。
- 写死在 Python 里的大段常量（工具 schema、提示词、清单）不要手抄：写一个 `server/scripts/dump_<模块>.py`，导出成
  `assets/<模块>.json` 或 `src/prompts/<模块>/*.txt`，再 `include_str!`。`assets/schemas.json` 是共用的，不要重新生成它。
- 工具（registry 里的）的 schema、顺序、where、gated、parallel 已经在 `builtin.rs` 登记好了，只要实现 `tool_xxx(c: Call) -> Out`。
- 写法照着已经迁好的模块：`db::rows / row / exec / insert`、`py::*`（Python 的真假值、json.dumps 的格式、按字符截断）、
  `t!("中文", n = 3)`、`server::broadcast(json!(...))`、接口报错用 `web::ApiError`（ValueError → 400）。
- 每个模块配一份接口测试 `contract/test_<模块>.py`：先在 Python 版上跑通，再跑 Rust 版。

## 发现的问题

- **run_code 用哪个 Python（设计，sandbox.rs）**：Python 版用服务自己的解释器（sys.executable）。Rust 版没有自带 Python，按顺序找：
  环境变量 `OSA_PYTHON` → PATH 上的 `python3`（macOS 的 `/usr/bin/python3` 只有 `xcode-select -p` 成功才算，不然一跑就弹安装框）
  → `uv python find` → 都没有、但有 uv 时第一次用到才 `uv python install`。找到后问它一次 `sys.prefix / base_prefix / exec_prefix`
  （不带 -S，venv 的 prefix 才对），沙箱里放开读这几个目录和 `realpath(exe)/../..`，和 Python 版一样。
  Linux 的 unshare 退路的序幕脚本本身要 Python，找不到 Python 时只用 bwrap。
- **Linux 沙箱（bwrap / unshare）只照搬了代码，还没在 Linux 上跑过**：这台开发机是 macOS，接口测试只验证了 sandbox-exec。上 lsp 之前要在 Linux 上跑一遍 contract/test_sandbox.py。

（重写时读到的、Python 版里可能不对的地方，记在这里，单独决定改不改）

- **群聊交接的竞争**（groups.py `after_task`）：先 `unexpect()` 再拿群的锁。dispatch 填了 handoff_to 的任务如果在
  派任务的那一轮还没说完时就做完了，排在后面、被 @ 到的那个 Bot 就不再跳过，先按“被 @”说一次，交接时又说一次。
  Rust 版照搬了（任务做得快更容易碰上）；接口测试里让假任务慢 0.5 秒避开。改法：unexpect 挪到拿到锁之后。

- **对话里直接操作电脑时开机失败**（router.py `_direct`）：Python 版 `c.start()` 报错会让这一整轮对话出错（“出错了：…”）；
  Rust 版把它当成这个工具的结果（“电脑操作失败：…”），模型能接着说。Rust 的工具返回值里放不下异常，就这一处和 Python 不一样。

## 顺手修的

- 首屏：`/assets` 加 `Cache-Control: public, max-age=31536000, immutable`，接口和静态文件按 `Accept-Encoding` 压缩（br / gzip）。
  Python 版没有，lsp 的 Caddy 上 osa / demo 两个站点也没开 `encode`，1.29 MB 的 JS 原样传（gzip 后 407 KB）。
  这一条不改接口，Rust 版直接带上。
