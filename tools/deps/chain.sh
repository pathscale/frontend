#!/bin/sh
# chain.sh [std|reg|anyhow|serde_json|tokio]: read a dependency chain from source with
# frontend-facts, each crate once, in dependency order, writing each crate's metadata for the
# crates after it. `std` reads std's closure; `reg` reads the three registry crates' closures
# (run `std` first). Every crate is read `--items`.
#
#   LIBRARY   a rust-src `library/` of frontend's upstream era, with its `vendor/`
#   REGISTRY  a cargo registry source directory holding the registry crates named below
#   OUT       where metadata (`meta/lib<name>.rmeta`), facts and logs go
#   FACTS     the `frontend-facts` binary (default `target/release/frontend-facts`)
#   ARCH      the target architecture std's build script would report (default `aarch64`)
set -u
here=$(cd "$(dirname "$0")" && pwd)
B=${FACTS:-target/release/frontend-facts}
L=${LIBRARY:?set LIBRARY to a rust-src library/ directory}
V=$L/vendor
R=${REGISTRY:?set REGISTRY to a cargo registry src directory}
S=${OUT:?set OUT to an output directory}
M=$S/meta
mkdir -p $M $S/out
CB="--extern noprelude:compiler_builtins=$M/libcompiler_builtins.rmeta"
# rd NAME ROOT EDITION ARGS...
rd() {
  name=$1; root=$2; ed=$3; shift 3
  $here/../bounded.sh 600 /usr/bin/time -p $B --root $root --edition $ed --items ${LIBFLAG:-} "$@" --emit-metadata $M/lib$name.rmeta $name > $S/out/$name.json 2> $S/out/$name.err
  rc=$?
  t=$(grep '^real' $S/out/$name.err | tail -1)
  n=$(grep -c '^error' $S/out/$name.err)
  echo "$name rc=$rc $t errors=$n"
  if [ $rc -ne 0 ]; then grep '^error' -A2 $S/out/$name.err | head -12; exit 1; fi
}
case "${1:-std}" in
std)
LIBFLAG=--standard-library
rd core $L/core/src/lib.rs 2024
rd compiler_builtins $L/compiler-builtins/compiler-builtins/src/lib.rs 2024 --cfg 'feature="compiler-builtins"' --cfg 'feature="unmangled-names"' --extern core=$M/libcore.rmeta
rd alloc $L/alloc/src/lib.rs 2024 --extern core=$M/libcore.rmeta $CB
rd cfg_if $V/cfg-if-1.0.4/src/lib.rs 2018 --cfg 'feature="rustc-dep-of-std"' --cfg 'feature="core"' --extern core=$M/libcore.rmeta $CB
rd libc $V/libc-0.2.189/src/lib.rs 2021 --cfg 'feature="rustc-dep-of-std"' --cfg 'feature="align"' --cfg 'feature="rustc-std-workspace-core"' --extern rustc_std_workspace_core=$M/libcore.rmeta $CB
rd rustc_demangle $V/rustc-demangle-0.1.28/src/lib.rs 2015 --cfg 'feature="rustc-dep-of-std"' --cfg 'feature="core"' --extern core=$M/libcore.rmeta $CB
rd hashbrown $V/hashbrown-0.17.1/src/lib.rs 2024 --cfg 'feature="rustc-dep-of-std"' --cfg 'feature="nightly"' --cfg 'feature="core"' --cfg 'feature="alloc"' --cfg 'feature="rustc-internal-api"' --extern core=$M/libcore.rmeta --extern alloc=$M/liballoc.rmeta $CB
rd unwind $L/unwind/src/lib.rs 2024 --extern core=$M/libcore.rmeta --extern libc=$M/liblibc.rmeta --extern noprelude:rustc_std_workspace_core=$M/libcore.rmeta $CB
rd panic_abort $L/panic_abort/src/lib.rs 2024 --extern core=$M/libcore.rmeta --extern alloc=$M/liballoc.rmeta $CB
rd std_detect $L/std_detect/src/lib.rs 2024 --extern core=$M/libcore.rmeta --extern alloc=$M/liballoc.rmeta --extern libc=$M/liblibc.rmeta --extern noprelude:rustc_std_workspace_core=$M/libcore.rmeta $CB
rd panic_unwind $L/panic_unwind/src/lib.rs 2024 --extern core=$M/libcore.rmeta --extern alloc=$M/liballoc.rmeta --extern libc=$M/liblibc.rmeta --extern unwind=$M/libunwind.rmeta --extern cfg_if=$M/libcfg_if.rmeta --extern noprelude:rustc_std_workspace_core=$M/libcore.rmeta $CB
rd std $L/std/src/lib.rs 2024 --cfg backtrace_in_libstd --env STD_ENV_ARCH=${ARCH:-aarch64} \
  --extern core=$M/libcore.rmeta --extern alloc=$M/liballoc.rmeta --extern libc=$M/liblibc.rmeta \
  --extern cfg_if=$M/libcfg_if.rmeta --extern hashbrown=$M/libhashbrown.rmeta --extern unwind=$M/libunwind.rmeta \
  --extern panic_abort=$M/libpanic_abort.rmeta --extern panic_unwind=$M/libpanic_unwind.rmeta --cfg 'feature="panic-unwind"' --extern std_detect=$M/libstd_detect.rmeta \
  --extern rustc_demangle=$M/librustc_demangle.rmeta \
  --extern noprelude:rustc_std_workspace_core=$M/libcore.rmeta $CB
LIBFLAG=
;;
esac
# std and everything std loads, for a crate that uses std.
STD="--extern std=$M/libstd.rmeta --extern noprelude:core=$M/libcore.rmeta --extern noprelude:alloc=$M/liballoc.rmeta $CB"
for d in libc cfg_if hashbrown unwind panic_abort panic_unwind std_detect rustc_demangle; do
  STD="$STD --extern noprelude:$d=$M/lib$d.rmeta"
done
STD="$STD --extern noprelude:rustc_std_workspace_core=$M/libcore.rmeta"
case "${1:-std}" in
reg|anyhow)
rd anyhow $R/anyhow-1.0.104/src/lib.rs 2021 --cfg 'feature="std"' --cfg 'feature="default"' $STD
;;
esac
case "${1:-std}" in
reg|serde_json)
# serde_core `include!`s a file its build script writes; frontend runs no build script, so it is written here.
SERDE_OUT=$S/out-serde_core; mkdir -p $SERDE_OUT
printf '#[doc(hidden)]\npub mod __private229 {\n    #[doc(hidden)]\n    pub use crate::private::*;\n}\n' > $SERDE_OUT/private.rs
rd serde_core $R/serde_core-1.0.229/src/lib.rs 2021 --cfg 'feature="std"' --cfg 'feature="result"' --cfg 'feature="default"' --env OUT_DIR=$SERDE_OUT --env CARGO_PKG_VERSION_PATCH=229 $STD
rd itoa $R/itoa-1.0.18/src/lib.rs 2021 $STD
rd memchr $R/memchr-2.8.3/src/lib.rs 2021 --cfg 'feature="std"' --cfg 'feature="alloc"' $STD
rd zmij $R/zmij-1.0.23/src/lib.rs 2021 $STD
rd serde_json $R/serde_json-1.0.151/src/lib.rs 2021 --cfg 'feature="std"' --cfg 'feature="default"' --cfg 'fast_arithmetic="64"' \
  --extern serde_core=$M/libserde_core.rmeta --extern itoa=$M/libitoa.rmeta --extern memchr=$M/libmemchr.rmeta --extern zmij=$M/libzmij.rmeta $STD
;;
esac
case "${1:-std}" in
reg|tokio)
LIBFLAG=--standard-library; rd rustc_literal_escaper $V/rustc-literal-escaper-0.0.8/src/lib.rs 2021 --cfg 'feature="rustc-dep-of-std"' --extern noprelude:rustc_std_workspace_core=$M/libcore.rmeta --extern noprelude:rustc_std_workspace_std=$M/libstd.rmeta $STD; LIBFLAG=
LIBFLAG=--standard-library; rd proc_macro $L/proc_macro/src/lib.rs 2024 --extern rustc_literal_escaper=$M/librustc_literal_escaper.rmeta $STD; LIBFLAG=
PM="--extern proc_macro=$M/libproc_macro.rmeta --extern noprelude:rustc_literal_escaper=$M/librustc_literal_escaper.rmeta"
rd unicode_ident $R/unicode-ident-1.0.26/src/lib.rs 2021 $STD
rd proc_macro2 $R/proc-macro2-1.0.107/src/lib.rs 2021 --cfg 'feature="proc-macro"' --cfg 'feature="default"' --cfg wrap_proc_macro --extern unicode_ident=$M/libunicode_ident.rmeta $PM $STD
rd quote $R/quote-1.0.47/src/lib.rs 2021 --cfg 'feature="proc-macro"' --cfg 'feature="default"' --extern proc_macro2=$M/libproc_macro2.rmeta --extern noprelude:unicode_ident=$M/libunicode_ident.rmeta $PM $STD
SYNF=""; for f in derive parsing printing clone-impls proc-macro full default; do SYNF="$SYNF --cfg feature=\"$f\""; done
rd syn $R/syn-3.0.6/src/lib.rs 2021 $SYNF --extern proc_macro2=$M/libproc_macro2.rmeta --extern quote=$M/libquote.rmeta --extern unicode_ident=$M/libunicode_ident.rmeta $PM $STD
rd tokio_macros $R/tokio-macros-2.7.2/src/lib.rs 2021 --proc-macro --extern proc_macro2=$M/libproc_macro2.rmeta --extern quote=$M/libquote.rmeta --extern syn=$M/libsyn.rmeta --extern noprelude:unicode_ident=$M/libunicode_ident.rmeta $PM $STD
rd pin_project_lite $R/pin-project-lite-0.2.17/src/lib.rs 2018 $STD
TOKF=""; for f in sync macros rt; do TOKF="$TOKF --cfg feature=\"$f\""; done
rd tokio $R/tokio-1.53.1/src/lib.rs 2021 $TOKF --extern pin_project_lite=$M/libpin_project_lite.rmeta --extern tokio_macros=$M/libtokio_macros.rmeta --extern noprelude:proc_macro=$M/libproc_macro.rmeta $STD
;;
esac
