"""把 Python 版里写死的工具 schema 导出成 JSON，Rust 版编进二进制（逐字一样，模型服务的前缀缓存才命中）。
    uv run python server/scripts/dump_assets.py
用临时的数据目录和配置，不碰真的数据库。"""
import json, os, sys, tempfile
from pathlib import Path
import yaml

ROOT = Path(__file__).resolve().parents[2]
tmp = Path(tempfile.mkdtemp())
cfg = yaml.safe_load((ROOT / "config.yaml").read_text())
cfg["memory"]["db"] = str(tmp / "osa.db")
cfg["mcp_servers"] = {}
(tmp / "config.yaml").write_text(yaml.safe_dump(cfg, allow_unicode=True))
os.environ["OSA_CONFIG"] = str(tmp / "config.yaml")
os.environ["OSA_DATA_DIR"] = str(tmp)
sys.path.insert(0, str(ROOT))

from osa import registry, router, botadmin, builtin  # noqa
from osa.computer import tools as ctools
from osa import providers

tools = []
for t in registry.TOOLS.values():
    tools.append({"name": t.name, "schema": t.schema, "where": sorted(t.where), "gated": t.gated, "parallel": t.parallel,
                  "describe": t.describe, "group": t.group, "has_when": t.when is not None, "owner": t.owner})
out = {
    "registry": tools,
    "router_tools": router.ROUTER_TOOLS,
    "message_bot": router.MESSAGE_BOT,
    "handoff": router.HANDOFF,
    "computer_schemas": ctools.SCHEMAS,
    "desk_schemas": ctools.DESK_SCHEMAS,
    "provider_presets": providers.PRESETS,
    "botadmin_primary": botadmin.tools_for({"is_primary": True}),
    "botadmin_other": botadmin.tools_for({"is_primary": False}),
}
p = ROOT / "server" / "crates" / "osa" / "assets" / "schemas.json"
p.write_text(json.dumps(out, ensure_ascii=False, indent=1))
print(p, len(tools), "registry tools", len(out["router_tools"]), "router tools")

# 提示词模板：原样写成文本文件（str.format 的 {{ }} 换回 { }，Rust 版只替换认识的 {名字}）
P = ROOT / "server" / "crates" / "osa" / "src" / "prompts"
P.mkdir(exist_ok=True)
for name, text in {"router": router.ROUTER_PROMPT, "state": router.STATE_PROMPT, "group": router.GROUP_PROMPT,
                   "dm_turn": router.DM_TURN, "primary_rule": router.PRIMARY_RULE, "botadmin_turn": router.BOTADMIN_TURN}.items():
    assert "{{" not in text and "}}" not in text, name
    (P / f"{name}.txt").write_text(text)
# 和 Rust 里手抄的几句对一下
import re
src = (ROOT / "server" / "crates" / "osa" / "src" / "router.rs").read_text()
for name in ("ATTACH_BLIND", "ATTACH_SEE", "OTHER_RULE", "REPLY_TURN", "REPLY_SHOWN", "REPLY_OWED", "BOTADMIN_OWED", "BOTADMIN_EDITED"):
    want = getattr(router, name)
    m = re.search(rf'pub const {name}: &str = "((?:[^"\\]|\\.)*)";', src)
    got = m and m.group(1).encode().decode("unicode_escape").encode("latin1").decode() if m else None
    if got != want:
        print("提示词不一样：", name, repr(got)[:120], repr(want)[:120])

from osa import auth
(P / "login.html").write_text(auth.PAGE)
(P / "computer_body.txt").write_text(ctools._BODY)
