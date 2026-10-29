#!/bin/sh
# Runs a built daemon's `dev hello`, which auto-starts `serve`: Linux binaries
# in a container (alpine for musl, debian:10 for gnu), Darwin ones directly.
set -eu

target="$1"
binary="$(cd "$(dirname "$0")/.." && pwd)/target/dist/bitplane-sessiond-$target"
[ -x "$binary" ] || chmod +x "$binary"

case "$target" in
x86_64-*-linux-*) platform=linux/amd64 ;;
aarch64-*-linux-*) platform=linux/arm64 ;;
esac

case "$target" in
*-linux-musl | *-linux-gnu)
	case "$target" in
	*-musl) image=alpine:3.22 ;;
	*) image=debian:10 ;;
	esac
	docker run --rm --platform "$platform" \
		--volume "$binary:/usr/local/bin/bitplane-sessiond:ro" \
		--env TMPDIR=/tmp \
		"$image" sh -c '
			bitplane-sessiond dev hello | tee /tmp/hello.json &&
				grep -q "\"name\": \"bitplane-sessiond\"" /tmp/hello.json ||
				{ cat "$HOME"/.local/state/bitplane/sessiond/*/sessiond.log; exit 1; }'
	;;
*-apple-darwin)
	dir="$(mktemp -d /tmp/sd-smoke.XXXXXX)"
	stop() {
		for pidfile in "$dir"/run/bitplane/sessiond/*/sessiond.pid; do
			[ -s "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null || true
		done
		rm -rf "$dir"
	}
	trap stop EXIT
	XDG_RUNTIME_DIR="$dir/run" XDG_STATE_HOME="$dir/state" \
		"$binary" dev hello | tee "$dir/hello.json"
	grep -q '"name": "bitplane-sessiond"' "$dir/hello.json" ||
		{ cat "$dir"/state/bitplane/sessiond/*/sessiond.log; exit 1; }
	;;
*)
	echo "unknown target $target" >&2
	exit 1
	;;
esac
