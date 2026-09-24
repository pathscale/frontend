#!/bin/sh
# check.sh FIXTURE...: check each fixture (`ok`, `e0599`, `e0061`, beside this script) against
# tokio and its closure, read by `chain.sh std` and `chain.sh tokio` into `$OUT/meta`. The
# answers are frontend-facts' `Checked` JSON, with the wall clock of each check.
#
#   OUT    the directory chain.sh wrote
#   FACTS  the `frontend-facts` binary (default `target/release/frontend-facts`)
set -u
here=$(cd "$(dirname "$0")" && pwd)
B=${FACTS:-target/release/frontend-facts}
S=${OUT:?set OUT to the directory chain.sh wrote}
M=$S/meta
D="--extern tokio=$M/libtokio.rmeta --extern std=$M/libstd.rmeta"
for d in core alloc compiler_builtins libc cfg_if hashbrown unwind panic_abort panic_unwind std_detect rustc_demangle pin_project_lite tokio_macros proc_macro rustc_literal_escaper; do
  D="$D --extern noprelude:$d=$M/lib$d.rmeta"
done
D="$D --extern noprelude:rustc_std_workspace_core=$M/libcore.rmeta"
mkdir -p $S/out
for f in "$@"; do
  # Through `sh -c`: the watchdog runs the command in the background, where stdin is /dev/null.
  "$here/../bounded.sh" 120 sh -c "exec /usr/bin/time -p $B --check --edition 2021 $D user < $here/$f.rs" > $S/out/check-$f.json 2> $S/out/check-$f.err
  echo "== $f rc=$? $(grep '^real' $S/out/check-$f.err)"
  head -c 900 $S/out/check-$f.json; echo
done
