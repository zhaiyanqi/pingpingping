#!/bin/sh
exec "$(dirname "$0")/build-macos-release.sh" --debug "$@"
