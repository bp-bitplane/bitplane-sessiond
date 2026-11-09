#!/bin/sh
# Runs a command with what building libghostty-vt needs: the vendored ghostty
# source in GHOSTTY_SOURCE_DIR and, on macOS, the xcrun shim that points
# Zig at a pre-26.4 SDK. See docs/build.md.
set -eu

scripts="$(cd "$(dirname "$0")" && pwd)"
GHOSTTY_SOURCE_DIR="$("$scripts/vendor-ghostty.sh")"
export GHOSTTY_SOURCE_DIR
if [ "$(uname -s)" = Darwin ]; then
	PATH="$scripts/macos-sdk:$PATH"
	export PATH
fi
exec "$@"
