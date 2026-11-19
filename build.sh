#!/usr/bin/env bash
# WinIsland 统一构建脚本
# 交叉编译 Windows x64，最终产物固定输出到 target/release/winisland.exe
# 交叉编译缓存放在系统缓存目录（~/.cache），不污染项目内的 target/
set -euo pipefail

cd "$(dirname "$0")"

export PATH="$HOME/.cargo/bin:$HOME/zig:$PATH"
BUILD_DIR="${WINISLAND_BUILD_DIR:-$HOME/.cache/winisland-cross-build}"

echo "[1/2] 交叉编译 x86_64-pc-windows-gnu ..."
CARGO_TARGET_DIR="$BUILD_DIR" cargo zigbuild --release --target x86_64-pc-windows-gnu

echo "[2/2] 复制产物到 target/release/winisland.exe ..."
mkdir -p target/release
cp "$BUILD_DIR/x86_64-pc-windows-gnu/release/winisland.exe" target/release/winisland.exe

echo "完成: $(pwd)/target/release/winisland.exe"
ls -lh target/release/winisland.exe
