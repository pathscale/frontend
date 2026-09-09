//! The two things `std` would otherwise supply to a final binary: a `#[panic_handler]` and the
//! `eh_personality` lang item.
//!
//! # Why this is a crate of its own
//!
//! It was half of `frontend::unwind_janky` until 2026-09-08, and that pairing made the whole test
//! suite of every dependent unbuildable. `unwind_janky` is a *library* - `rustc_span` and
//! `rustc_proc_macro` both depend on it for `catch`. A test target links `test`, which
//! links `std`, which defines `panic_impl` and `eh_personality`; a library that also defines them
//! is a duplicate lang item and nothing in the package compiles:
//!
//! ```text
//! error[E0152]: duplicate lang item in crate `std` (which `test` depends on): `panic_impl`
//!   = note: the lang item is first defined in crate `unwind_janky`
//! ```
//!
//! A lang item may be defined once per linked binary, so it belongs to the binary. The split is
//! that rule written down: `unwind_janky` declares the runtime symbols it calls and defines no
//! lang item, so any dependent may link `std` beside it, and this crate defines the lang items
//! and is named by the final binary alone.
//!
//! **Being a dependency is not enough to break a test build; being *named* is.** rustc loads a
//! crate when something refers to it, and registers lang items from the crates it loaded. So
//! this crate may sit in a library's `[dependencies]` while only that package's binary target
//! says `extern crate frontend_panic_runtime;`, and the library and test targets never load it.
//! Measured, not assumed: a `no_std` lib with an unnamed dependency defining `panic_impl` builds
//! its test target cleanly, and adding `extern crate` to that lib produces the E0152 above.
//!
//! # What is here
//!
//! `#[panic_handler]`, which formats the message and hands it to `__rust_start_panic`; the
//! `eh_personality` lang item in [`personality`]; the two `#[rustc_std_internal_symbol]` stubs
//! `panic_unwind` calls back into; and the `extern crate` lines for `panic_unwind` and `unwind`
//! that put the panic runtime and the libSystem unwinder in the binary.
//!
//! `catch` and `resume` are not here. They are the API, they define no lang item, and they stay
//! in `crates/unwind-janky`, whose header carries the argument for why any of this exists.

#![no_std]
#![feature(core_intrinsics, lang_items, panic_unwind, rustc_attrs, std_internals)]
#![allow(internal_features)]

extern crate alloc;
// The panic runtime. It exports `__rust_start_panic` and `__rust_panic_cleanup`, and it depends
// on `core`, `alloc`, `libc` and `unwind` - not on `std`. Naming it here is what puts those two
// symbols in the binary, which is also what makes `frontend::unwind_janky::catch` link.
extern crate panic_unwind;
// `_Unwind_RaiseException` and friends, over libSystem on this platform. `personality::gcc`
// refers to it as `uw`.
extern crate unwind;

mod personality;

use alloc::boxed::Box;
use alloc::string::String;
use core::any::Any;

/// The payload handed to the panic runtime.
///
/// `__rust_start_panic` takes `&mut dyn PanicPayload` and calls `take_box` to get the
/// `Box<dyn Any + Send>` that `__rust_panic_cleanup` will hand back to `frontend::unwind_janky::catch` on
/// the other side. Making that box a `String` is what lets `catch_fatal_errors` recognise its own
/// sentinel: it downcasts to `&'static str` and to `String`, and formatting a `panic!("{}", s)`
/// yields the latter.
struct StringPayload(Option<String>);

impl core::fmt::Display for StringPayload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0.as_deref().unwrap_or(""))
    }
}

// `take_box` leaves `None` behind, which is the "dummy default value" the trait's contract asks
// for, and `get` is only reached before it.
impl alloc::panicking::PanicPayload for StringPayload {
    fn take_box(&mut self) -> Box<dyn Any + Send> {
        Box::new(self.0.take().unwrap_or_default())
    }

    fn get(&mut self) -> &(dyn Any + Send) {
        self.0.get_or_insert_with(String::new)
    }

    /// The message is already a `String`, so borrowing it costs nothing. `panic_unwind` uses
    /// this to avoid an allocation on the raise path.
    fn as_str(&mut self) -> Option<&str> {
        self.0.as_deref()
    }
}

/// Write to stderr without an allocator or a formatter.
///
/// A panic that unwinds out of the top with nothing printed is a process that dies silently, and
/// this crate exists precisely to make failures loud. `eko` is not a dependency here -
/// this crate has none, on purpose, since it is linked into the final binary - so `write` is
/// declared directly. Short writes are not retried: this is the last thing the process does.
fn stderr(bytes: &[u8]) {
    unsafe extern "C" {
        fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    }
    // Safe: `bytes` is a valid slice and fd 2 is either open or the write fails harmlessly.
    unsafe {
        let _ = write(2, bytes.as_ptr(), bytes.len());
    }
}

/// `core` calls this on `panic!`. It builds the payload and starts the unwind.
///
/// This is `library/std/src/panicking.rs::panic_handler` with everything removed that needs an
/// operating system: no panic hook, no backtrace, no `panic_count`, no double-panic detection,
/// no abort-on-unwind guard. What is left is the part that matters here - format the message,
/// box it, and raise - because the only unwind this compiler performs on purpose is
/// `FatalError`, and `catch_fatal_errors` catches it two frames later.
///
/// **What is given up by not counting panics**: a panic *while panicking* will not be turned into
/// a clean abort, and `frontend::unwind_janky::catch` can be re-entered. Both were already true of the
/// bridge this replaces, and the ruling on this tree is that the daemon may die.
#[panic_handler]
fn panic_handler(info: &core::panic::PanicInfo<'_>) -> ! {
    let msg = alloc::format!("{}", info.message());

    stderr(b"panicked at ");
    if let Some(loc) = info.location() {
        stderr(loc.file().as_bytes());
        stderr(b":");
        let mut buf = [0u8; 20];
        stderr(itoa(loc.line() as u64, &mut buf));
    }
    stderr(b":\n  ");
    stderr(msg.as_bytes());
    stderr(b"\n");

    // Recorded before the raise, because `frontend::unwind_janky::resume` will replace this payload with
    // its own string on the way out and the original message would otherwise be lost.
    frontend::unwind_janky::record_panic(match info.location() {
        Some(loc) => alloc::format!("{} at {}:{}", msg, loc.file(), loc.line()),
        None => msg.clone(),
    });
    let mut payload = StringPayload(Some(msg));
    // Safe: `__rust_start_panic` is `panic_unwind`'s, the payload outlives the call because the
    // call diverges, and the code after it is unreachable unless the runtime failed to raise.
    unsafe {
        __rust_start_panic(&mut payload);
    }
    // Reached only if the unwinder could not raise, which it does not do on this platform.
    core::intrinsics::abort()
}

/// Decimal, without `format!`, for use before the payload exists.
fn itoa(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    if n == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &buf[i..]
}

unsafe extern "Rust" {
    /// `panic_unwind`'s. Wraps the payload in an `_Unwind_Exception` and calls
    /// `_Unwind_RaiseException`. `#[rustc_std_internal_symbol]` is load-bearing: without it the
    /// name goes through ordinary C mangling and the linker looks for a leading-underscore
    /// symbol nothing exports.
    #[rustc_std_internal_symbol]
    fn __rust_start_panic(payload: &mut dyn alloc::panicking::PanicPayload) -> u32;
}

// ---- the two symbols `panic_unwind` calls back into -------------------------------------------

/// Called by the panic runtime when FFI code catches a Rust panic and does not rethrow it.
///
/// `std`'s version aborts with "Rust panics must be rethrown", and so does this. Its comment
/// gives the reason as the panic count, which this tree does not keep - but the behaviour is
/// right for a second reason: an exception that was caught and dropped has left every frame
/// between the raise and the catch un-unwound, so there is no state to return to.
#[rustc_std_internal_symbol]
fn __rust_drop_panic() -> ! {
    stderr(b"fatal: a Rust panic was caught by foreign code and not rethrown\n");
    core::intrinsics::abort()
}

/// Called by the panic runtime when it catches an exception that is not a Rust panic.
///
/// A C++ or Objective-C exception reaching a Rust frame. `std` aborts and so does this: the
/// payload is not a `Box<dyn Any>` and there is nothing truthful to hand to
/// `frontend::unwind_janky::catch`.
#[rustc_std_internal_symbol]
fn __rust_foreign_exception() -> ! {
    stderr(b"fatal: a foreign exception reached Rust code, which cannot catch one\n");
    core::intrinsics::abort()
}

