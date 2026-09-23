#!/bin/sh
# Is a module of this crate the same code as the crate it was taken from?
#
#     scripts/compare-upstream-module.sh rustc_error_messages
#     scripts/compare-upstream-module.sh rustc_errors /path/to/compiler
#
# Prints the differences that are *code*, having normalised away the four things the collapse
# changed deliberately:
#
#   1. paths - `rustc_x::` upstream against `crate::rustc_x::` here;
#   2. crate attributes, which moved into one `src/lib.rs` from seventy;
#   3. `extern crate` lines, same reason;
#   4. `pub use <macro>;` re-exports, which exist because `#[macro_export]` puts a macro at the
#      crate root rather than in the module that defines it.
#
# Comments are excluded: they differ almost everywhere and none of it is behaviour.
#
# # Why `rg` and not `sed`
#
# `/usr/bin/sed` on macOS is BSD sed, which does not support `\b`. It does not error on it either
# - the expression matches nothing, the substitution silently does nothing, and every file then
# reads as divergent. That is measured, not hypothetical: it reported 1281 divergent files here
# when the real number was 97. `rg` has a real regex engine and is already installed. `grep` here
# is ugrep, which does support `\s`, but there is no reason to depend on which grep is on PATH.
set -eu

MODULE=${1:-}
UPSTREAM=${2:?name the upstream compiler directory (rust-lang/rust compiler/)}

if [ -z "$MODULE" ]; then
    echo "usage: $0 <module> <upstream compiler dir>" >&2
    echo "   eg: $0 rustc_error_messages" >&2
    exit 2
fi

command -v rg > /dev/null || { echo "needs ripgrep: brew install ripgrep" >&2; exit 2; }

HERE=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

ours="$HERE/src/$MODULE/mod.rs"
[ -f "$ours" ] || ours="$HERE/src/$MODULE.rs"
theirs="$UPSTREAM/$MODULE/src/lib.rs"

[ -f "$ours" ]   || { echo "no module here: $MODULE" >&2; exit 2; }
[ -f "$theirs" ] || { echo "no crate upstream: $UPSTREAM/$MODULE" >&2; exit 2; }

# Strip the path prefixes both sides spell differently, then drop comments, crate attributes,
# `extern crate` and the macro re-exports, then blank lines.
normalise() {
    rg --passthru -N -r '' '\bcrate::' "$1" \
        | rg --passthru -N -r '' '\bfrontend_diag_template::' \
        | rg --passthru -N -r '' '\brustc_diag_template::' \
        | rg --passthru -N -r '' '\bfrontend_arena::' \
        | rg --passthru -N -r '' '\brustc_[a-z_0-9]+::' \
        | rg -v '^\s*(//|///|//!)' \
        | rg -v '^\s*#!\[' \
        | rg -v '^\s*extern crate' \
        | rg -v '^\s*pub use [a-z_][a-z_0-9]*;\s*$' \
        | rg -v '^\s*$'
}

a=$(mktemp) ; b=$(mktemp)
trap 'rm -f "$a" "$b"' EXIT INT TERM

normalise "$theirs" > "$a"
normalise "$ours"   > "$b"

if diff -u --label upstream --label here "$a" "$b"; then
    echo "$MODULE: code identical"
else
    echo
    echo "$MODULE: code differs - the lines above are real, not path or comment noise"
    exit 1
fi
