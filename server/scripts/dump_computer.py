"""电脑部分写死在 Python 里的提示词导出成文本文件（Rust 版 include_str!）。
    uv run python server/scripts/dump_computer.py"""
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
from osa.computer import tools  # noqa: E402

P = ROOT / "server" / "crates" / "osa" / "src" / "prompts" / "computer"
P.mkdir(parents=True, exist_ok=True)
(P / "desk_hint.txt").write_text(tools.DESK_HINT)
print(P)
