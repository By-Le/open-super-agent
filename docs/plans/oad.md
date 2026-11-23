# 文档改用 Open Agent Docs

2026-11-23 定的：OSA 的文档交给 [Open Agent Docs](https://github.com/kadaliao/open-agent-docs)（OAD，0.3.1 起）。
旧资料库里的 Markdown 页面和 Notion 都不再用；资料库里的网页（HTML 报告）、小应用、电脑里的文件照旧。

## 要得到什么

- 装 OSA 时一并装好文档：服务器上一个 `docker compose up -d`，桌面 App 打开就有，不用另外部署、另外登录。
- Bot 和人一起写文档：每个 Bot 在 OAD 里是一个 Agent，写的段落、评论记在它名下；人在评论里 @ 它，它就被叫醒处理。
- 页面能分享给外面的人看：`<OSA 地址>/s/<token>`，不用登录、只读、随时关。

## 怎么接

```
浏览器 / 桌面 App / 手机 ──> osa-server ──(header 登录，密钥只在 osa 手里)──> OAD（只听内网 / 127.0.0.1）
                               │  /api/v1/*（含协同编辑的 WebSocket）、/s/*：原样转发
                               │  Bot 的文档工具：OAD 的 /mcp，每个 Bot 用自己的 Agent token
                               └─ Rust SDK：建 / 改 / 撤 Bot 的 Agent，收评论通知叫醒 Bot
```

- **OAD 单独一个进程，由 osa-server 自己当子进程起**，两种模式一样：只听 127.0.0.1、随机端口，数据在 `<数据目录>/docs`，
  header 密钥第一次生成、存在 `docs/.osa-header-secret`（600），退出了 3 秒后重起，服务停时一起停（SIGTERM，等它存盘）。
  - 程序从哪来：Docker 镜像（`Dockerfile.rust`）从官方镜像 `ghcr.io/kadaliao/open-agent-docs:<版本>` 拷进来；桌面 App 当 externalBin 带着
    （`desktop/scripts/build-server.sh` 从发布页下载、核对 SHA256）。`OSA_DOCS_BIN` 可以指定。
  - 一开始打算在 compose 里另起一个 `oad` 服务。改成子进程是因为：OAD 只从环境变量读 header 密钥、镜像是 distroless，
    另起服务就得让用户在 `.env` 里配一个共享密钥；子进程不用配任何东西，compose 不多服务，桌面 App 的外壳也不用改。
  - 想连别处已经在跑的 OAD：`OSA_DOCS_URL` + `OSA_DOCS_SECRET`。`OSA_DOCS=off` 不用文档（lsp 的演示服务器）。
  - 不挂进 osa-server 进程（发版绑死、要跟 oad-core 内部接口走），不用 SDK 的 launcher（运行时去 GitHub 下载，绕过 App 签名）。
- **登录**：OAD 用 `OAD_AUTH=header`。osa-server 转发 `/api/v1/*` 时先过 OSA 自己的登录和同源检查，再带上 `X-OAD-Secret`、
  `X-OAD-User-Id: owner`，去掉 Origin（OSA 已经查过）；浏览器发来的 `X-OAD-*` 一律丢掉。第一个用户自动是工作区所有者。
- **分享**：`/s/*` 不用登录，原样转发（资料库网页以前的分享也是 `/s/<token>`：token 是网页的就照旧由前端显示）。
  链接用浏览器当前的地址拼；设了对外地址时 OAD 的 `OAD_PUBLIC_URL` 用它，通知里 OAD 自己的地址换成站内相对地址。
  桌面 App 本机模式的分享链接只有这台电脑能开（生成时提示）。
- **Bot 的工具**：OSA 用每个 Bot 的 token 连 OAD 的 `/mcp`，工具注册成 `docs__<工具>`。工具说明由 OAD 维护。
  对话（Router）给读、找、按块改、建页面、回评论；后台任务全部。`save_page` 写 Markdown 时存成文档（模板、技能里写的 save_page 不用改），
  html 和小应用照旧进资料库；`read_page` 认文档的字符串编号。
- **叫醒**：OSA 建一个 admin 的 Agent「OSA」，用它的通知流收整个工作区的通知；按通知里的 Agent 找到 Bot，
  在它的私聊里叫醒（kind=doc_comment，提示词是通知里的 prompt）。这一轮结束后 Bot 没在评论里回的，OSA 用它的 token 把回答发到那串评论里，
  再确认通知（没确认的，重连或重启后会再推）。
- **备份**：`.osabak` 里带 `docs/oad.db`（另做一份一致快照）和 `docs/blobs/`，搜索索引不带（文档服务启动时自己补）。
  Agent 的 token 在备份里是明文（整个备份是加密的，和 secrets.json 一样），恢复时用新机器的主密钥重新加密；备份里没有文档就清掉这些 Agent，文档从空的开始。
  恢复时原来的 `data/docs` 留成 `docs.before-restore-<时间>`。连的是别处的 OAD（`OSA_DOCS_URL`）时不备份文档。
- **外部 Agent**（dsh Bots、Claude Code）：现在不对外开 `/mcp`。要的话在同一个转发里加这一条路径。

## 分几步

1. 服务端：`docs.rs`（起 OAD、转发、Bot 的 Agent、工具、通知叫醒）；`save_page` 的 Markdown 进文档；旧的 Markdown 页面第一次启动时搬进 OAD。
2. 网页：资料库分“文档 / 网页 / 文件”，文档用 OAD 的编辑器和评论栏（`@open-agent-docs/editor`，按需加载）。
3. 部署：`Dockerfile.rust` 带上 OAD，根目录 compose 改用它；lsp 的演示服务器关掉文档；桌面 App 带 `open-agent-docs`。
4. lsp 上线：备份后清掉旧资料库页面、断开 Notion、Bot 的说明 / 定时任务 / 技能 / 记忆改成用文档；删掉 docs.cclsp.top。

只做 Rust 版（lsp 和桌面 App 都是它）；Python 版不接 OAD，接口测试里文档相关的只跑 Rust。
