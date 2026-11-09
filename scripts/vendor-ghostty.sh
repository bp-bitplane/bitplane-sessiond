#!/bin/sh
# Fetches ghostty at the commit libghostty-vt-sys pins into vendor/ghostty,
# for GHOSTTY_SOURCE_DIR. See docs/build.md.
set -eu

GHOSTTY_COMMIT=a887df42c56f6de86c0fe6da9c4eeca37931e083
cd "$(dirname "$0")/.."
dir=vendor/ghostty

if [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null || true)" != "$GHOSTTY_COMMIT" ]; then
	rm -rf "$dir"
	mkdir -p "$dir"
	git -C "$dir" init --quiet
	git -C "$dir" fetch --quiet --depth 1 https://github.com/ghostty-org/ghostty.git "$GHOSTTY_COMMIT"
	git -C "$dir" -c advice.detachedHead=false checkout --quiet FETCH_HEAD
fi
echo "$(pwd)/$dir"
