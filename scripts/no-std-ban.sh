#!/bin/sh
# STD IS BANNED. This fails if anything shipped from this repository reaches for it.
#
# No baseline and no exception list, deliberately. A ratchet against a count is the right
# instrument while a migration runs and the wrong one afterwards, because a ratchet with a
# non-zero baseline is a permission slip.
#
# Two things are checked, because there are two ways to link std.
#
#   1. `extern crate std;` - the only way a `#![no_std]` crate reaches std. Banned outright,
#      including under `#[cfg(test)]`: a test that needs std builds a std crate.
#
#   2. A `std::` path in code. The compiler already rejects these, since without the extern crate
#      there is nothing to resolve, but its error names a missing crate rather than a broken rule.
#      This one says which rule.
#
# **This check is not sufficient and is not meant to be.** A crate links std through its
# dependencies without ever spelling `std::`, which is what `no-std-link-check.sh` is for: that
# one reads the actual link line and cannot be fooled, this one reads the source and can be. Run
# both and believe the other one.
#
# `frontend_macros` is out of scope, on a structural ground rather than a judgement: a proc-macro
# crate runs inside the host compiler at build time and is never linked into anything shipped, so
# its `std` costs the artefact nothing.
#
# POSIX sh. No process substitution, no arrays, no pipefail.
set -eu
cd "$(dirname "$0")/.."
fail=0
SCAN="src frontend_diag_template/src"

# ---- 1. `extern crate std` --------------------------------------------------------------------
if grep -rn --include='*.rs' '^[[:space:]]*extern crate std;' $SCAN 2>/dev/null; then
    echo
    echo "STD IS BANNED: the lines above put it back."
    echo
    echo "If something needs an operating system it goes through \`ekostd\`; if it needs an"
    echo "allocation, \`alloc\` has it; if it needs neither, \`core\` does."
    fail=1
fi

# ---- 2. a `std::` path in code ----------------------------------------------------------------
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
: > "$work/hits"

find $SCAN -name '*.rs' 2>/dev/null > "$work/files" || true
if [ ! -s "$work/files" ]; then
    echo "no-std-ban: no .rs files found. Run this from the repository root." >&2
    exit 2
fi

while read -r file; do
    # Strip comments and string literals before matching. A `sed` per line cannot do this: rustc's
    # help text spans several lines inside one string literal, so quote state has to be carried
    # across lines. `awk` keeps `instr` between records, which is the whole trick.
    #
    # This matters because rustc quotes `std::` paths at its users constantly - "use
    # `std::env::var` instead" - and those are advice about *their* code. A checker that flagged
    # them would be teaching people to emit suggestions nobody can follow.
    awk '
        { line = $0 }
        line ~ /^[[:space:]]*\/\// { print ""; next }
        {
            out = ""; i = 1; n = length(line)
            while (i <= n) {
                c = substr(line, i, 1)
                if (instr) {
                    if (c == "\\") { i += 2; continue }
                    if (c == "\"") { instr = 0 }
                    i++; continue
                }
                if (c == "\"") { instr = 1; i++; continue }
                if (c == "/" && substr(line, i + 1, 1) == "/") { break }
                out = out c; i++
            }
            print out
        }
    ' "$file" > "$work/stripped"
    # `sym::std` is a symbol comparison rather than a use: it asks whether an identifier in the
    # program being compiled is spelled `std`.
    if grep -E '(^|[^:_[:alnum:]])std::' "$work/stripped" | grep -qv 'sym::std'; then
        printf '%s\n' "$file" >> "$work/hits"
    fi
done < "$work/files"

if [ -s "$work/hits" ]; then
    echo
    echo "STD IS BANNED: these files name a std:: path in code."
    sed 's/^/  /' "$work/hits"
    echo
    echo "\`core\` and \`alloc\` cover almost all of it. The operating system is \`ekostd\`."
    fail=1
fi

if [ "$fail" = 0 ]; then
    echo "std is banned and absent: no extern crate std, no std:: path in code."
fi
exit "$fail"
