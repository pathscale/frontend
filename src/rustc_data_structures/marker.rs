// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

// `DynSend` and `DynSync` were `unsafe auto trait`s with a table of negative impls (raw
// pointers, `Rc`, cells) and hand-written positive impls (atomics, locks, arenas,
// collections). Upstream they gate what the parallel frontend may share between threads
// once `sync::is_dyn_thread_safe()` is true.
//
// Auto traits and negative impls are unstable, and this crate runs a compile on the
// caller's thread (no threads, no pool), so there is nothing for them to gate. They are
// now ordinary unsafe marker traits implemented for every type, which keeps every
// `T: DynSend` bound and `impl Fn() + DynSync` argument compiling unchanged. Two things
// could not be kept: an ordinary trait cannot be added to a trait object, so the former
// `dyn Trait + DynSend + DynSync` types are now `dyn Trait`; and the per-type impls
// elsewhere in the tree (`TyCtxt`, `Lock`, `RawList`, ...) were deleted, since they would
// conflict with the blanket impl. `FromDyn` in `sync.rs` no longer trusts these traits.
//
// If a thread pool ever comes back, this is the file to revisit: a universal `DynSend`
// states nothing, and the checks it used to perform would need a real mechanism again.

#[diagnostic::on_unimplemented(message = "`{Self}` doesn't implement `DynSend`. \
            Add it to `crate::rustc_data_structures::marker` or use `IntoDynSyncSend` if it's already `Send`")]
/// A type that could be sent across threads when `sync::is_dyn_thread_safe()` is true.
/// Implemented for every type; see the note at the top of this file.
pub unsafe trait DynSend {}

#[diagnostic::on_unimplemented(message = "`{Self}` doesn't implement `DynSync`. \
            Add it to `crate::rustc_data_structures::marker` or use `IntoDynSyncSend` if it's already `Sync`")]
/// A type that could be shared across threads when `sync::is_dyn_thread_safe()` is true.
/// Implemented for every type; see the note at the top of this file.
pub unsafe trait DynSync {}

// SAFETY: no code in this crate moves or shares a value across threads, so these markers
// promise nothing that can be broken. See the note at the top of this file.
unsafe impl<T: ?Sized> DynSend for T {}
unsafe impl<T: ?Sized> DynSync for T {}

pub fn assert_dyn_sync<T: ?Sized + DynSync>() {}
pub fn assert_dyn_send<T: ?Sized + DynSend>() {}
pub fn assert_dyn_send_val<T: ?Sized + DynSend>(_t: &T) {}
pub fn assert_dyn_send_sync_val<T: ?Sized + DynSync + DynSend>(_t: &T) {}

// A wrapper to convert a struct that is already a `Send` or `Sync` into
// an instance of `DynSend` and `DynSync`, since the compiler cannot infer
// it automatically in some cases. (e.g. Box<dyn Send / Sync>)
// With the blanket impls above the wrapper adds nothing, but it is kept so its callers
// and its `Deref` stay unchanged.
#[derive(Copy, Clone)]
pub struct IntoDynSyncSend<T: ?Sized>(pub T);

impl<T> core::ops::Deref for IntoDynSyncSend<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> core::ops::DerefMut for IntoDynSyncSend<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}
