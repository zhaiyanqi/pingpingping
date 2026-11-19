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
    1:--size)
        cargo build --profile release-small --locked --target-dir target
        profile=release-small
        bundle=dist/PingPingPing.app
        ;;
    1:--debug)
        cargo build --locked --target-dir target
        profile=debug
        bundle=dist/debug/PingPingPing.app
        ;;
    *)
        printf 'Usage: %s [--debug | --size]\n' "$0" >&2
        exit 2
        ;;
esac

mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp packaging/Info.plist "$bundle/Contents/Info.plist"
if [ -n "${PINGPINGPING_RELEASE_VERSION:-}" ]; then
    # macOS bundle version fields use the numeric core of the release version.
    # macOS 应用包的版本字段只使用发布版本号的数字部分。
    version=${PINGPINGPING_RELEASE_VERSION%%[-+]*}
    plutil -replace CFBundleShortVersionString -string "$version" "$bundle/Contents/Info.plist"
    plutil -replace CFBundleVersion -string "$version" "$bundle/Contents/Info.plist"
fi
cp packaging/pingpingping.icns "$bundle/Contents/Resources/"
cp packaging/LICENSE-LUCIDE.txt "$bundle/Contents/Resources/"
cp "target/$profile/pingpingping" "$bundle/Contents/MacOS/"
codesign --force --sign - "$bundle"
codesign --verify --strict "$bundle"
printf 'Built: %s/%s\n' "$PWD" "$bundle"
