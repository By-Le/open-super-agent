"""导出 sandbox.py 的 unshare 序幕脚本、teach.py 的提示词（Rust 版 include_str!）。
    uv run python server/scripts/dump_sandbox.py"""
import os, sys, tempfile
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
from osa import sandbox, teach  # noqa

P = ROOT / "server" / "crates" / "osa" / "src" / "prompts"
(P / "sandbox" / "prelude.py").write_text(sandbox._PRELUDE)
(P / "teach" / "draft.txt").write_text(teach.DRAFT_PROMPT)
print("ok")
