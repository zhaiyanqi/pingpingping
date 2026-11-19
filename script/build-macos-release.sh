#!/bin/sh
set -eu

# Build and sign a local app bundle from any working directory.
# 从任意工作目录构建并签名本地应用包。
if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'Run this script on macOS.' >&2
    exit 1
fi

cd "$(dirname "$0")/.."
case "$#:${1-}" in
    0:)
        cargo build --release --locked --target-dir target
        profile=release
        bundle=dist/PingPingPing.app
        ;;
    1:--debug)
        cargo build --locked --target-dir target
        profile=debug
        bundle=dist/debug/PingPingPing.app
        ;;
    *)
        printf 'Usage: %s [--debug]\n' "$0" >&2
        exit 2
        ;;
esac

mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp packaging/Info.plist "$bundle/Contents/Info.plist"
cp packaging/pingpingping.icns "$bundle/Contents/Resources/"
cp "target/$profile/pingpingping" "$bundle/Contents/MacOS/"
codesign --force --sign - "$bundle"
codesign --verify --strict "$bundle"
printf 'Built: %s/%s\n' "$PWD" "$bundle"
