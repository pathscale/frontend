//! A binary that exists only so the link line can be read.
//!
//! `no-std-link-check.sh` asks rustc what it hands the linker, which is the only account of what
//! is in an artefact that cannot be argued with. A **library** has no link line: it is a pile of
//! objects and metadata, and whether `libstd` ends up beside it is decided later, by whoever links
//! it. So a source-level ban is the only check a library repository can run on itself, and a
//! source-level ban is precisely the one that a dependency can walk around without ever spelling
//! `std::`.
//!
//! This is the smallest thing that closes that gap. It links `frontend`, which drags in the whole
//! dependency graph, and it links `frontend_panic_runtime` for the two lang items a `no_std`
//! binary must have. If anything in that graph pulls the standard library, `libstd-*.rlib` appears
//! on this binary's link line and the check fails naming it.
//!
//! It is not a tool and does nothing useful when run. `main` returns immediately.

#![no_std]
// `fn main` is std's: the shim that calls it lives in `lang_start`. The C runtime calls the
// symbol below directly instead, and its return value is the exit status.
#![no_main]

// The `#[panic_handler]` and the `eh_personality` lang item. rustc only loads a crate that
// something names, so this line is what puts them in the binary.
#[allow(unused_extern_crates)]
extern crate frontend_panic_runtime;

// A `no_std` binary has no allocator until it names one, and `frontend` allocates on every path.
// Without this the build stops at "no global memory allocator found but one is required", which
// is the linker's way of saying the same thing this probe exists to ask about.
#[global_allocator]
static GLOBAL: eko::heap::Malloc = eko::heap::Malloc;

// Touch the library, so it is genuinely linked rather than resolved and dropped. Any public item
// would do; a span is the cheapest thing that is definitely reachable.
#[unsafe(no_mangle)]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    core::hint::black_box(core::mem::size_of::<frontend::rustc_span::Span>()) as i32
}
