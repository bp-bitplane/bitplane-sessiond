#!/bin/sh
# Builds the daemon for the bootstrap targets into target/dist and checks each
# binary. With no arguments: all six on macOS, the four Linux ones elsewhere.
# See docs/build.md.
set -eu

ZIG_VERSION=0.15.2
GLIBC_FLOOR=2.27
cd "$(dirname "$0")/.."

if [ $# -eq 0 ]; then
	set -- x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
		x86_64-unknown-linux-musl aarch64-unknown-linux-musl
	if [ "$(uname -s)" = Darwin ]; then
		set -- "$@" x86_64-apple-darwin aarch64-apple-darwin
	fi
fi

require_zig() {
	found="$("${CARGO_ZIGBUILD_ZIG_PATH:-zig}" version 2>/dev/null || echo none)"
	if [ "$found" != "$ZIG_VERSION" ]; then
		echo "zig $ZIG_VERSION is required, found $found" >&2
		exit 1
	fi
}

mkdir -p target/dist
for target in "$@"; do
	rustup target add "$target" >/dev/null 2>&1
	case "$target" in
	*-linux-gnu)
		require_zig
		cargo zigbuild --locked --release --target "$target.$GLIBC_FLOOR"
		;;
	*-linux-musl)
		require_zig
		cargo zigbuild --locked --release --target "$target"
		;;
	*-apple-darwin)
		cargo build --locked --release --target "$target"
		;;
	*)
		echo "not a bootstrap target: $target" >&2
		exit 1
		;;
	esac
	cp "target/$target/release/bitplane-sessiond" "target/dist/bitplane-sessiond-$target"
	scripts/check-binary.sh "target/dist/bitplane-sessiond-$target" "$target"
done
