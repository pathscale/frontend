//! A binary that exists only so the link line can be read.
//!
//! `no-std-link-check.sh` asks rustc what it hands the linker, which is the only account of what
//! is in an artefact that cannot be argued with. A **library** has no link line: whether `libstd`
//! ends up beside it is decided by whoever links it, so a source-level ban is the only check a
//! library repository can run on itself, and a dependency can walk around that one without ever
//! spelling `std::`.
//!
//! This is the smallest thing that closes that gap. It links `frontend`, which drags in the whole
//! dependency graph; if anything in it pulls the standard library, `libstd-*.rlib` appears on this
//! binary's link line and the check fails naming it. It builds on stable, so it aborts on a panic
//! rather than unwinding. It is not a tool, and `main` returns immediately.

#![no_std]
// `fn main` is std's: the shim that calls it lives in `lang_start`. The C runtime calls the
// symbol below directly instead, and its return value is the exit status.
#![no_main]

use core::panic::PanicInfo;

unsafe extern "C" {
    fn abort() -> !;
}

/// Stable `no_std` binaries supply their own panic handler. This one aborts: without the
/// nightly personality routine there is no unwinding to do.
#[panic_handler]
fn panic(_: &PanicInfo<'_>) -> ! {
    // SAFETY: `abort` takes no arguments and never returns.
    unsafe { abort() }
}

// A `no_std` binary has no allocator until it names one, and `frontend` allocates on every path.
#[global_allocator]
static GLOBAL: eko::heap::Malloc = eko::heap::Malloc;

// Touch the library, so it is genuinely linked rather than resolved and dropped. A span is the
// cheapest public item that is definitely reachable.
#[unsafe(no_mangle)]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    core::hint::black_box(core::mem::size_of::<frontend::rustc_span::Span>()) as i32
}
