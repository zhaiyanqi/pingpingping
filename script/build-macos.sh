#!/bin/sh
set -eu

# Build and sign a local app bundle from any working directory.
# 从任意工作目录构建并签名本地应用包。
if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'Run this script on macOS.' >&2
    exit 1
fi

cd "$(dirname "$0")/.."
cargo build --release --locked --target-dir target

bundle=dist/PingPingPing.app
mkdir -p "$bundle/Contents/MacOS"
cp packaging/Info.plist "$bundle/Contents/Info.plist"
cp target/release/pingpingping "$bundle/Contents/MacOS/"
codesign --force --sign - "$bundle"
codesign --verify --strict "$bundle"
printf 'Built: %s/%s\n' "$PWD" "$bundle"
