"""应用市场随项目发布的清单（osa/catalog/apps/*.yaml）编进 Rust 版：生成 server/crates/osa/src/catalog/files.rs。
加了、删了清单要重新跑一次：uv run python server/scripts/dump_catalog.py（改清单内容不用）。"""
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
apps = sorted((ROOT / "osa" / "catalog" / "apps").glob("*.yaml"))
lines = ["//! 生成的文件（server/scripts/dump_catalog.py），别手改。", "", "pub const BUILTIN: &[(&str, &str)] = &["]
for p in apps:
    lines.append(f'    ("{p.stem}", include_str!("../../../../../osa/catalog/apps/{p.name}")),')
lines.append("];")
out = ROOT / "server" / "crates" / "osa" / "src" / "catalog" / "files.rs"
out.write_text("\n".join(lines) + "\n")
print(out, len(apps))
