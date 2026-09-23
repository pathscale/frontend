#!/bin/sh
# STD MUST NOT BE LINKED. This asks the linker, not the source.
#
# `no-std-ban.sh` is the source-level ban: no `extern crate std;`, no `std::` path in code. It can
# pass while the artefact still contains `libstd`, because third-party crates pull it in without
# any of our files naming it. A ban that cannot see that is not the ban that matters.
#
# So this one reads the actual link line. `--print=link-args` makes rustc report exactly what it
# hands `cc`, and that is the only account of what is in a binary that cannot be argued with.
#
# # Why there is a binary in a library repository
#
# A library has no link line. It is objects and metadata, and whether `libstd` ends up beside it is
# decided later by whoever links it. `link-probe/` is for exactly this: the smallest `no_std`
# binary that links `frontend`, built on stable with `panic = "abort"`. It does nothing when run.
# Its whole purpose is to have a link line.
#
# **It builds.** That is the cost: minutes, against seconds for the source check. Run it before
# believing anything about what this crate links.
set -eu
cd "$(dirname "$0")/.."

command -v cargo > /dev/null || { echo "no-std-link-check: no \`cargo\` on PATH" >&2; exit 2; }

err=$(mktemp)
trap 'rm -f "$err"' EXIT INT TERM

# Keep cargo's exit status and its stderr, and treat them separately from an empty result. Those
# are different failures: a build that broke has a message worth printing, and a tree that was
# already built prints no link args at all because cargo never re-invokes rustc.
args=$(cargo rustc --manifest-path link-probe/Cargo.toml --bin link-probe -- --print=link-args 2>"$err") || {
    echo "no-std-link-check: the build failed, so there is no link line to read." >&2
    echo "cargo said:" >&2
    tail -30 "$err" >&2
    exit 2
}

if [ -z "$args" ]; then
    # Nothing to rebuild. Dirty just the probe and ask again; it is one small file.
    touch link-probe/src/main.rs
    args=$(cargo rustc --manifest-path link-probe/Cargo.toml --bin link-probe -- --print=link-args 2>"$err") || {
        echo "no-std-link-check: the build failed on the forced relink." >&2
        tail -30 "$err" >&2
        exit 2
    }
fi

if [ -z "$args" ]; then
    echo "no-std-link-check: cargo succeeded but rustc printed no link args even after a forced" >&2
    echo "relink. That is not a std verdict either way; investigate before believing anything." >&2
    exit 2
fi

linked=$(printf '%s' "$args" | tr ' ' '\n' | grep -oE 'libstd-[a-f0-9]*\.rlib' | sort -u || true)

if [ -n "$linked" ]; then
    echo "STD IS LINKED:"
    printf '%s\n' "$linked" | sed 's/^/  /'
    echo
    echo "No source file names it. It arrives through dependencies, and the honest way to find"
    echo "which is to ask cargo rather than to guess:"
    echo
    echo "    cargo tree -e normal -i <crate>"
    echo
    echo "A crate that supports no_std needs \`default-features = false\` where it is declared."
    echo "A crate that does not support it has to be replaced or dropped."
    exit 1
fi

echo "std is not linked: the probe binary contains no libstd."
printf '%s' "$args" | tr ' ' '\n' | grep -oE 'lib[a-z_0-9]+-[a-f0-9]+\.rlib' | sed 's/-[a-f0-9]*\.rlib//' \
    | sort -u | head -12 | sed 's/^/  sysroot rlib: /'
