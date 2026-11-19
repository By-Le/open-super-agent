"""把事件来源的写死的信息（标题、说明、参数、场景、示例）导出成 JSON，Rust 版编进二进制。
    uv run python server/scripts/dump_routines.py
只导出数据；check / parse / match / summary / validate 这些逻辑在 server/crates/osa/src/sources/ 里用 Rust 写。"""
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

from osa import registry, builtin  # noqa: F401  导入即登记来源

out = []
for s in registry.SOURCES.values():
    out.append({"id": s.id, "title": s.title, "description": s.description, "mode": s.mode, "fields": s.fields,
                "category": s.category, "icon": s.icon, "on": s.on, "every": s.every, "min_every": s.min_every,
                "fire_first": s.fire_first, "setup": s.setup, "sample": s.sample, "scenarios": s.scenarios,
                "has_summary": s.summary is not None, "has_validate": s.validate is not None,
                "has_available": s.available is not None})
p = ROOT / "server" / "crates" / "osa" / "assets" / "sources.json"
p.write_text(json.dumps(out, ensure_ascii=False, indent=1))
print(p, len(out), "sources:", ", ".join(s["id"] for s in out))
