#!/bin/sh
# Build a standard library this frontend can actually read.
#
# # Why you need this
#
# Crate metadata records the version string of the compiler that wrote it, and this frontend
# accepts only metadata from its own vintage. It was pruned from a specific upstream commit, and
# that commit sits *between two published nightlies*, so no toolchain you can install will do:
#
#     a sysroot older than this fork  ->  an assertion inside `rustc_serialize`
#     a sysroot newer than this fork  ->  an `ExplicitBug` inside `rustc_hir_typeck`
#
# Neither failure mentions a sysroot. The first reads as a corrupt file and the second as a
# compiler bug, which is why this script exists rather than a sentence in the README.
#
# # What it needs
#
# An upstream checkout at the commit in `UPSTREAM.md`, and a nightly able to compile rustc's own
# source as the stage0. Both are named below and neither is downloaded behind your back.
#
# No Python. Upstream drives its build through `x.py`, which is a Python script; `src/bootstrap`
# underneath it is an ordinary Rust program and is run directly here.
#
#     ./scripts/build-sysroot.sh [path-to-rust-checkout]
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
HOST=${HOST:-$(rustc -vV | sed -n 's/^host: //p')}
UPSTREAM_COMMIT=4b7e3a76d8df78960dc7c65cad43f5da1dac8ade
RUST=${1:-${RUST_CHECKOUT:-$ROOT/../rust}}

if [ ! -d "$RUST/src/bootstrap" ]; then
    echo "build-sysroot: no rust checkout at \`$RUST\`." >&2
    echo >&2
    echo "    git clone https://github.com/rust-lang/rust.git $RUST" >&2
    echo "    git -C $RUST checkout $UPSTREAM_COMMIT" >&2
    echo >&2
    echo "Then run this again, or pass the path: ./scripts/build-sysroot.sh <path>" >&2
    exit 2
fi

at=$(git -C "$RUST" rev-parse HEAD 2>/dev/null || echo unknown)
if [ "$at" != "$UPSTREAM_COMMIT" ]; then
    echo "build-sysroot: \`$RUST\` is at $at" >&2
    echo "               this frontend was pruned from $UPSTREAM_COMMIT" >&2
    echo >&2
    echo "A sysroot from a different commit may still build and will then fail at *use* time," >&2
    echo "as a serializer assertion or a type-checking ICE. Check it out, or set" >&2
    echo "SYSROOT_ANY_COMMIT=1 if you know why you are doing this." >&2
    [ "${SYSROOT_ANY_COMMIT:-}" = "1" ] || exit 2
fi

command -v cargo > /dev/null || { echo "build-sysroot: no \`cargo\` on PATH" >&2; exit 2; }

# The stage0 that compiles rustc's source. Named rather than downloaded: bootstrap's default is to
# fetch a beta toolchain, which puts a second, unpinned compiler in the build.
STAGE0=${STAGE0:-$(rustc --print sysroot)}
[ -x "$STAGE0/bin/rustc" ] || { echo "build-sysroot: no stage0 rustc at $STAGE0/bin/rustc" >&2; exit 2; }

# `bootstrap` needs its sibling `rustc` shim on disk beside it, so build the bins and run the
# binary rather than using `cargo run`, which builds only one of them and then panics saying so.
( cd "$RUST/src/bootstrap" && cargo build --bins -q )

# Bootstrap assumes these exist and panics with a bare "No such file or directory" from a stamp
# write if they do not, which is what a fresh or deleted `build/` looks like.
mkdir -p "$RUST/build/$HOST" "$RUST/build/cache" "$RUST/build/tmp"

( cd "$RUST" && ./src/bootstrap/target/debug/bootstrap build --stage 1 library \
    --set build.rustc="$STAGE0/bin/rustc" \
    --set build.cargo="$STAGE0/bin/cargo" )

SYSROOT=$RUST/build/$HOST/stage1
echo
echo "sysroot: $SYSROOT"
version=$("$SYSROOT/bin/rustc" --version 2>/dev/null | sed 's/^rustc //')
echo "it reports: ${version:-unknown}"
echo
echo 'The CFG_VERSION in .cargo/config.toml must carry that string exactly:'
echo "    CFG_VERSION = \"${version:-unknown}\""
current=$(sed -n 's/^CFG_VERSION = "\(.*\)"/\1/p' "$ROOT/.cargo/config.toml")
if [ "$current" = "$version" ]; then
    echo "and it does."
else
    echo "and it currently says \"$current\", which will not work."
fi
