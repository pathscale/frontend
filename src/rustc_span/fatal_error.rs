/// Used as a return value to signify a fatal error occurred.
// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

#[derive(Copy, Clone, Debug)]
#[must_use]
pub struct FatalError;

pub use crate::rustc_data_structures::FatalErrorMarker;

// Upstream had `impl !Send for FatalError` so that `std::panic::panic_any(FatalError)` could
// not compile. Negative impls are unstable, and a marker field would break every
// `FatalError` unit expression. The guard has nothing left to guard here: there is no
// `panic_any` without `std`, and `raise` panics with a `&str` sentinel, never the value.

impl FatalError {
    pub fn raise(self) -> ! {
        // The sentinel travels by unwinding to the `catch_fatal_errors` below. `panic!` is how
        // it starts: the payload is a `&str` rather than the `FatalErrorMarker` box `std`'s
        // `resume_unwind` carried, because `unwind_janky` cannot construct an arbitrary payload
        // - see its `resume` doc. `catch_fatal_errors` therefore recognises a fatal error by
        // *catching at all* rather than by downcasting, which is why nothing else may panic
        // through that frame and expect to be distinguished.
        panic!("{}", FATAL_SENTINEL)
    }
}

impl core::fmt::Display for FatalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "fatal error")
    }
}

impl core::error::Error for FatalError {}

/// The message `FatalError::raise` panics with.
///
/// Not a marker type. `crate::unwind_janky::resume` cannot re-raise an arbitrary payload, so the
/// sentinel has to be something a `&str` payload can carry, and this is it.
const FATAL_SENTINEL: &str = "rustc fatal error (compilation refused)";

/// Run a closure, turning a fatal error inside it into an `Err` rather than a dead process.
///
/// The compiler aborts a compilation it refuses by unwinding a sentinel from deep in the
/// frontend, and this is the frame that receives it. Twelve places raise one, including every
/// `emit_fatal`, so without this a program the compiler declines - a syntax error, a missing
/// file - takes the whole process down. That is not hypothetical: it is what compiling a hello
/// world did before this was restored.
///
/// **A panic that is not a fatal error is re-raised, not swallowed.** An ICE must not be
/// silently converted into "the compiler refused your program"; the two are different answers
/// and only one of them is the user's fault. The sentinel is compared as a string because
/// `unwind_janky` hands back a `&str` payload rather than the `FatalErrorMarker` box that
/// `std::panic::resume_unwind` preserved.
pub fn catch_fatal_errors<F: FnOnce() -> R, R>(f: F) -> Result<R, FatalError> {
    match crate::unwind_janky::catch(f) {
        Ok(r) => Ok(r),
        Err(payload) => {
            let fatal = payload
                .downcast_ref::<&'static str>()
                .is_some_and(|s| *s == FATAL_SENTINEL)
                || payload.downcast_ref::<String>().is_some_and(|s| s == FATAL_SENTINEL);
            if fatal {
                Err(FatalError)
            } else {
                // Someone else's panic. It belongs further out, not here.
                crate::unwind_janky::resume(payload)
            }
        }
    }
}
