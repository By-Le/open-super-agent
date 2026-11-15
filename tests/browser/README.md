# 资料库文档的浏览器检查

资料库块编辑器、评论、子页面、版本历史的手动回归脚本：用 Playwright 驱动系统 Chrome 点一遍，打印 `OK` / `FAIL`。
不进 CI（文件名不是 `test_*.py`，pytest 不收）；改了 `web/src/components/doc/`、`osa/pagedoc.py`、`osa/pagecomments.py` 这些以后跑一遍。

## 先起一个测试用的服务

在数据副本上起，别碰 `data/osa.db`（做法见 [开发说明](../../docs/development.md) 的“在数据副本上测试”）：

```sh
S=$(mktemp -d)
sqlite3 data/osa.db ".backup $S/osa.db"
# 复制 config.yaml 到 $S，db 改成 $S/osa.db，电脑设成 backend: local；副本里关掉频道和定时任务：
#   update channels set enabled=0; update routines set enabled=0;
(cd web && npm run build)
env -u INSTABOX_API_KEY OSA_DATA_DIR=$S OSA_CONFIG=$S/config.yaml uv run uvicorn osa.server:app --port 8766
```

服务不能开着登录（不设 `OSA_PASSWORD`）。测完把它停掉。

## 跑

```sh
uv run --with playwright python tests/browser/editor.py
```

| 脚本 | 看什么 | 花钱 |
|---|---|---|
| `editor.py` | 打开不保存、改一个词只发那一段（其余逐字节不变、软换行还是软换行）、回车加待办、斜杠菜单插提示框、两个窗口合并和冲突 | 否 |
| `editor_more.py` | 只开聊天不下载编辑器、地址和前进后退、保存不重拉整页、锁定、断网再连、打完字马上刷新不丢、分享页、浅色主题、手机宽度 | 否 |
| `tree.py` | 列表里新建子页面、路径和返回上一级、树展开记得住、挪到最上层、删父页面子页面上移 | 否 |
| `agent_live.py` | 外部 Agent 经 `/mcp` 建页面、按段改、写评论：开着的编辑器实时出现、“正在编辑”和作者条是 token 的名字、加的列表项接在原列表里；页面列表、设置里的列表和撤销，撤销后 401 | 否 |
| `history.py` | 版本列表、和现在比（按字标出）、恢复删掉的段（用回原来的编号）、只看这一版、回到这一版 | 否 |
| `comments_live.py` | 选中文字评论 → 叫醒写这段的 Bot → 它改页面、在评论里回复 | 是（一次对话） |
| `bot_edit_live.py` | 让 Bot 在对话里 `read_page` + `edit_page`，开着的编辑器实时出现“正在编辑”、闪一下、作者条 | 是（一次对话） |
| `handoff_live.py` | 群里一个 Bot 写页面、交接给另一个，后者在同一页面上用 `edit_page` 接着改 | 是（两三轮对话） |

- 花钱的几个要真的模型：起服务前把 key 导进环境（服务读不了副本里的保险库）。
- 脚本按副本里现有的 Bot 写：1 号 Nova（主 Bot）、2 号 Radar、3 号 Researcher。别的数据要改脚本里的编号和名字。
- `OSA_E2E_BASE` 换地址（默认 `http://127.0.0.1:8766`），`OSA_E2E_OUT` 换截图目录（默认系统临时目录）。
- Playwright 点完马上按键会用旧的选区（点击后的选区是异步交给编辑器的），脚本里点完都等了 0.3 秒，新写的也照这样。
