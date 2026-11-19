#!/usr/bin/env bash
# 准备桌面 App 要带的服务，放进 desktop/build/：
#   bin/osa-server-<triple>  Rust 版服务（一个文件），Tauri 当 externalBin 放进 Contents/MacOS 并签名
#   app/                     服务的根目录：config.yaml、构建好的前端（osa/static）、本机电脑的镜像文件
# App 不带 Python：uv（uvx 应用、run_code 要的 Python）第一次用到时才下载（见 server/crates/osa/src/runtime.rs）。
# 用法：desktop/scripts/build-server.sh [aarch64|x86_64]   不写就是这台机器的架构
# 先构建好前端（cd web && npm run build）。需要 Rust 和对应的 target（rustup target add x86_64-apple-darwin）。
set -euo pipefail

ARCH="${1:-$(uname -m)}"
[ "$ARCH" = "arm64" ] && ARCH=aarch64
case "$ARCH" in aarch64|x86_64) ;; *) echo "不支持的架构：$ARCH" >&2; exit 1 ;; esac
TRIPLE="$ARCH-apple-darwin"

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/desktop/build"
cd "$ROOT"
[ -f osa/static/dist/index.html ] || { echo "先构建前端：cd web && npm run build" >&2; exit 1; }

echo "==> osa-server ($TRIPLE)"
(cd server && cargo build --release --locked --bin osa-server --target "$TRIPLE")
rm -rf "$OUT/bin" "$OUT/app" "$OUT/runtime"
mkdir -p "$OUT/bin" "$OUT/app/osa/computer"
cp "server/target/$TRIPLE/release/osa-server" "$OUT/bin/osa-server-$TRIPLE"

echo "==> 根目录"
cp config.yaml pyproject.toml LICENSE "$OUT/app/"
rsync -a osa/static "$OUT/app/osa/"
rsync -a osa/computer/image "$OUT/app/osa/computer/"

echo "==> 试一下能不能起来"
RUN=()
[ "$ARCH" != "$(uname -m | sed 's/arm64/aarch64/')" ] && RUN=(arch -"$ARCH")   # Apple 芯片上打 Intel 版：用 Rosetta 试
TMP="$(mktemp -d)"
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
(cd "$OUT/app" && exec env OSA_ROOT="$OUT/app" OSA_DATA_DIR="$TMP" OSA_PASSWORD=x ${RUN[@]+"${RUN[@]}"} "$OUT/bin/osa-server-$TRIPLE" --port "$PORT" >"$TMP/log" 2>&1) &
PID=$!
ok=
for _ in $(seq 50); do curl -fsS "http://127.0.0.1:$PORT/manifest.json" -o /dev/null 2>/dev/null && { ok=1; break; }; sleep 0.1; done
kill "$PID" 2>/dev/null; wait "$PID" 2>/dev/null || true
[ -n "$ok" ] || { cat "$TMP/log" >&2; rm -rf "$TMP"; echo "起不来" >&2; exit 1; }
rm -rf "$TMP"
echo ok
du -sh "$OUT/bin" "$OUT/app"
