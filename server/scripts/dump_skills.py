"""技能市场（osa/catalog/skills）和内置 Bot 模板（osa/templates）编进 Rust 版：导出成 assets/skills.json。
    uv run python server/scripts/dump_skills.py"""
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
market = {}
for d in sorted((ROOT / "osa" / "catalog" / "skills").iterdir()):
    if d.is_dir():
        market[d.name] = {p.relative_to(d).as_posix(): p.read_text() for p in sorted(d.rglob("*")) if p.is_file()}
templates = {p.stem: p.read_text() for p in sorted((ROOT / "osa" / "templates").glob("*.yaml"))}
out = ROOT / "server" / "crates" / "osa" / "assets" / "skills.json"
out.write_text(json.dumps({"market": market, "templates": templates}, ensure_ascii=False, indent=1))
print(out, len(market), "market skills", len(templates), "templates")
