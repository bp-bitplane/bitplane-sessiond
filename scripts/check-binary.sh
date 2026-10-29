#!/bin/sh
# Checks a built daemon: musl is fully static, gnu needs no glibc newer than
# the floor, Darwin has the right architecture.
set -eu

GLIBC_FLOOR=2.27
binary="$1"
target="$2"

readelf() {
	host="$(rustc -vV | sed -n 's/^host: //p')"
	tool="$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-readobj"
	[ -x "$tool" ] || rustup component add llvm-tools >/dev/null 2>&1
	"$tool" --elf-output-style=GNU "$@"
}

fail() {
	echo "$binary: $*" >&2
	exit 1
}

[ -f "$binary" ] || fail "no such file"

case "$target" in
*-linux-musl)
	headers="$(readelf --program-headers --dynamic-table "$binary")"
	if echo "$headers" | grep -q INTERP; then
		fail "has a dynamic loader; musl builds must be static"
	fi
	if echo "$headers" | grep -q NEEDED; then
		fail "needs shared libraries; musl builds must be static"
	fi
	;;
*-linux-gnu)
	versions="$(readelf --version-info "$binary")"
	newest="$(echo "$versions" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' |
		awk -F. '{ v = $1 * 1000 + $2; if (v > max) { max = v; s = $0 } } END { print s }')"
	[ -n "$newest" ] || fail "references no glibc symbol versions"
	if awk -v a="$newest" -v b="$GLIBC_FLOOR" 'BEGIN {
		split(a, x, "."); split(b, y, ".")
		exit !(x[1] * 1000 + x[2] > y[1] * 1000 + y[2])
	}'; then
		fail "needs glibc $newest, newer than the $GLIBC_FLOOR floor"
	fi
	;;
*-apple-darwin)
	arch="${target%%-*}"
	[ "$arch" = aarch64 ] && arch=arm64
	file "$binary" | grep -q "Mach-O 64-bit executable $arch" ||
		fail "is not a $arch Mach-O executable"
	;;
*)
	fail "unknown target $target"
	;;
esac

echo "$binary: ok"
