"""资料库、小应用的常量导出成文本文件，Rust 版 include_str!（逐字一样）。
    uv run python server/scripts/dump_library.py"""
import os, sys, tempfile
from pathlib import Path
import yaml

ROOT = Path(__file__).resolve().parents[2]
tmp = Path(tempfile.mkdtemp())
cfg = yaml.safe_load((ROOT / "config.yaml").read_text())
cfg["memory"]["db"] = str(tmp / "osa.db")
(tmp / "config.yaml").write_text(yaml.safe_dump(cfg, allow_unicode=True))
os.environ["OSA_CONFIG"] = str(tmp / "config.yaml")
os.environ["OSA_DATA_DIR"] = str(tmp)
sys.path.insert(0, str(ROOT))
from osa import applets, library  # noqa

P = ROOT / "server" / "crates" / "osa" / "src" / "prompts" / "library"
P.mkdir(parents=True, exist_ok=True)
(P / "raw_csp.txt").write_text(library.RAW_CSP)
(P / "applet_sdk.js").write_text(applets.SDK_JS)
print("ok")
