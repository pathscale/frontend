// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::alloc::Allocator;
use core::marker::PointeeSized;

#[diagnostic::on_unimplemented(message = "`{Self}` doesn't implement `DynSend`. \
            Add it to `crate::rustc_data_structures::marker` or use `IntoDynSyncSend` if it's already `Send`")]
// This is an auto trait for types which can be sent across threads if `sync::is_dyn_thread_safe()`
// is true. These types can be wrapped in a `FromDyn` to get a `Send` type. Wrapping a
// `Send` type in `IntoDynSyncSend` will create a `DynSend` type.
pub unsafe auto trait DynSend {}

#[diagnostic::on_unimplemented(message = "`{Self}` doesn't implement `DynSync`. \
            Add it to `crate::rustc_data_structures::marker` or use `IntoDynSyncSend` if it's already `Sync`")]
// This is an auto trait for types which can be shared across threads if `sync::is_dyn_thread_safe()`
// is true. These types can be wrapped in a `FromDyn` to get a `Sync` type. Wrapping a
// `Sync` type in `IntoDynSyncSend` will create a `DynSync` type.
pub unsafe auto trait DynSync {}

// Same with `Sync` and `Send`.
unsafe impl<T: DynSync + ?Sized + PointeeSized> DynSend for &T {}

macro_rules! impls_dyn_send_neg {
    ($([$t1: ty $(where $($generics1: tt)*)?])*) => {
        $(impl$(<$($generics1)*>)? !DynSend for $t1 {})*
    };
}

// Consistent with `std`
//
// `std::env::Args`/`ArgsOs` and `std::env::VarsOs` had negative impls here; there are no such
// types now. `eko::env::args()` hands back an owned `Vec<Vec<u8>>` and `var_os` an
// owned `Option<Vec<u8>>`, so there is no borrowing iterator left to keep off another thread.
// `std::io::StdoutLock`/`StderrLock` are gone for the same reason: `eko::file::stdout()`
// returns a `Stream`, which is a bare fd and holds no lock (see the note on `file::stdout`).
impls_dyn_send_neg!(
    [*const T where T: ?Sized + PointeeSized]
    [*mut T where T: ?Sized + PointeeSized]
    [core::ptr::NonNull<T> where T: ?Sized + PointeeSized]
    [alloc::rc::Rc<T, A> where T: ?Sized, A: Allocator]
    [alloc::rc::Weak<T, A> where T: ?Sized, A: Allocator]
    [eko::thread::Guard<'_, T> where T: ?Sized]
    [eko::thread::ReadGuard<'_, T> where T: ?Sized]
    [eko::thread::WriteGuard<'_, T> where T: ?Sized]
);

macro_rules! already_send {
    ($([$ty: ty])*) => {
        $(unsafe impl DynSend for $ty where Self: Send {})*
    };
}

// These structures are already `Send`.
already_send!(
    [core::sync::atomic::AtomicBool][core::sync::atomic::AtomicUsize][core::sync::atomic::AtomicU8]
        [core::sync::atomic::AtomicU32][eko::file::Stream]
        [eko::file::Error][eko::file::File][core::panic::Location<'_>][crate::rustc_arena::DroplessArena]
        [crate::rustc_data_structures::memmap::Mmap]
        [crate::rustc_data_structures::owned_slice::OwnedSlice]
        [crate::rustc_serialize::opaque::FileEncoder<'_>]
        // `eko::thread::Condvar` is a `pthread_cond_t` behind an `UnsafeCell`, and the
        // query latch holds one per waiter. It carries no data of its own, which is what makes
        // the assertion here trivially true - `already_send!` asserts `Self: Send` rather than
        // granting it, and the wrapper declares that impl beside the type.
        [eko::thread::Condvar]
        // The diagnostic emitter's destination. It was `Box<dyn std::io::Write + Send>` when the
        // snippet renderer owned an `anstream`; a plain emitter formats text, so the sink is a
        // `core::fmt::Write`. The `+ Send` bound on the trait object is what makes this sound -
        // `already_send!` asserts rather than grants.
        [alloc::boxed::Box<dyn core::fmt::Write + Send>]
);

#[cfg(target_has_atomic = "64")]
already_send!([core::sync::atomic::AtomicU64]);

macro_rules! impl_dyn_send {
    ($($($attr: meta)* [$ty: ty where $($generics2: tt)*])*) => {
        $(unsafe impl<$($generics2)*> DynSend for $ty {})*
    };
}

impl_dyn_send!(
    [core::sync::atomic::AtomicPtr<T> where T]
    [eko::thread::Mutex<T> where T: ?Sized+ DynSend]
    // `std::sync::mpsc::Sender` was here. There is no `mpsc` in this build - `mpsc` appears
    // nowhere else in the compiler now that `rustc_thread_pool` is gone - so the impl named a
    // type that no longer exists. Restore it with the channel if one comes back.
    [alloc::sync::Arc<T> where T: ?Sized + DynSync + DynSend]
    [alloc::sync::Weak<T> where T: ?Sized + DynSync + DynSend]
    [eko::thread::LazyLock<T, F> where T: DynSend, F: DynSend]
    // `std::collections::HashSet`/`HashMap` were here; they convert to `hashbrown`, which the
    // next two lines already declare, so the converted lines would have been duplicate impls.
    [hashbrown::HashSet<K, S> where K: DynSend, S: DynSend]
    [hashbrown::HashMap<K, V, S> where K: DynSend, V: DynSend, S: DynSend]
    [alloc::collections::BTreeMap<K, V, A> where K: DynSend, V: DynSend, A: core::alloc::AllocatorClone + DynSend]
    [Vec<T, A> where T: DynSend, A: core::alloc::Allocator + DynSend]
    [Box<T, A> where T: ?Sized + DynSend, A: core::alloc::Allocator + DynSend]
    [crate::rustc_data_structures::sync::RwLock<T> where T: DynSend]
    [crate::rustc_data_structures::tagged_ptr::TaggedRef<'a, P, T> where 'a, P: Sync, T: Send + crate::rustc_data_structures::tagged_ptr::Tag]
    [crate::rustc_arena::TypedArena<T> where T: DynSend]
    [hashbrown::HashTable<T> where T: DynSend]
    [indexmap::IndexSet<V, S> where V: DynSend, S: DynSend]
    [indexmap::IndexMap<K, V, S> where K: DynSend, V: DynSend, S: DynSend]
    [thin_vec::ThinVec<T> where T: DynSend]
    [smallvec::SmallVec<A> where A: smallvec::Array + DynSend]
);

macro_rules! impls_dyn_sync_neg {
    ($([$t1: ty $(where $($generics1: tt)*)?])*) => {
        $(impl$(<$($generics1)*>)? !DynSync for $t1 {})*
    };
}

// Consistent with `std`
//
// `std::env::Args`/`ArgsOs`, `std::env::VarsOs` and the `mpsc` `Sender`/`Receiver` had negative
// impls here; none of those types exist in this build. See the note on `impls_dyn_send_neg!`.
impls_dyn_sync_neg!(
    [*const T where T: ?Sized + PointeeSized]
    [*mut T where T: ?Sized + PointeeSized]
    [core::cell::Cell<T> where T: ?Sized]
    [core::cell::RefCell<T> where T: ?Sized]
    [core::cell::UnsafeCell<T> where T: ?Sized]
    [core::ptr::NonNull<T> where T: ?Sized + PointeeSized]
    [alloc::rc::Rc<T, A> where T: ?Sized, A: Allocator]
    [alloc::rc::Weak<T, A> where T: ?Sized, A: Allocator]
    [core::cell::OnceCell<T> where T]
);

macro_rules! already_sync {
    ($([$ty: ty])*) => {
        $(unsafe impl DynSync for $ty where Self: Sync {})*
    };
}

// These structures are already `Sync`.
already_sync!(
    [core::sync::atomic::AtomicBool][core::sync::atomic::AtomicUsize][core::sync::atomic::AtomicU8]
        [core::sync::atomic::AtomicU32][eko::file::Error][eko::file::File][core::panic::Location<'_>]
        [crate::rustc_data_structures::memmap::Mmap]
        [crate::rustc_data_structures::owned_slice::OwnedSlice]
        // See the note beside this type in `already_send!`.
        [eko::thread::Condvar]
);

// Use portable AtomicU64 for targets without native 64-bit atomics
#[cfg(target_has_atomic = "64")]
already_sync!([core::sync::atomic::AtomicU64]);

#[cfg(not(target_has_atomic = "64"))]
already_sync!([portable_atomic::AtomicU64]);

macro_rules! impl_dyn_sync {
    ($($($attr: meta)* [$ty: ty where $($generics2: tt)*])*) => {
        $(unsafe impl<$($generics2)*> DynSync for $ty {})*
    };
}

impl_dyn_sync!(
    [core::sync::atomic::AtomicPtr<T> where T]
    [eko::thread::OnceLock<T> where T: DynSend + DynSync]
    [eko::thread::Mutex<T> where T: ?Sized + DynSend]
    [alloc::sync::Arc<T> where T: ?Sized + DynSync + DynSend]
    [alloc::sync::Weak<T> where T: ?Sized + DynSync + DynSend]
    [eko::thread::LazyLock<T, F> where T: DynSend + DynSync, F: DynSend]
    // `std::collections::HashSet`/`HashMap` were here; they convert to `hashbrown`, which the
    // next two lines already declare, so the converted lines would have been duplicate impls.
    [hashbrown::HashSet<K, S> where K: DynSync, S: DynSync]
    [hashbrown::HashMap<K, V, S> where K: DynSync, V: DynSync, S: DynSync]
    [alloc::collections::BTreeMap<K, V, A> where K: DynSync, V: DynSync, A: core::alloc::AllocatorClone + DynSync]
    [Vec<T, A> where T: DynSync, A: core::alloc::Allocator + DynSync]
    [Box<T, A> where T: ?Sized + DynSync, A: core::alloc::Allocator + DynSync]
    [crate::rustc_data_structures::sync::RwLock<T> where T: DynSend + DynSync]
    [crate::rustc_data_structures::sync::WorkerLocal<T> where T: DynSend]
    [crate::rustc_data_structures::intern::Interned<'a, T> where 'a, T: DynSync]
    [crate::rustc_data_structures::tagged_ptr::TaggedRef<'a, P, T> where 'a, P: Sync, T: Sync + crate::rustc_data_structures::tagged_ptr::Tag]
    [parking_lot::lock_api::Mutex<R, T> where R: DynSync, T: ?Sized + DynSend]
    [parking_lot::lock_api::RwLock<R, T> where R: DynSync, T: ?Sized + DynSend + DynSync]
    [hashbrown::HashTable<T> where T: DynSync]
    [indexmap::IndexSet<V, S> where V: DynSync, S: DynSync]
    [indexmap::IndexMap<K, V, S> where K: DynSync, V: DynSync, S: DynSync]
    [smallvec::SmallVec<A> where A: smallvec::Array + DynSync]
    [thin_vec::ThinVec<T> where T: DynSync]
);

pub fn assert_dyn_sync<T: ?Sized + PointeeSized + DynSync>() {}
pub fn assert_dyn_send<T: ?Sized + PointeeSized + DynSend>() {}
pub fn assert_dyn_send_val<T: ?Sized + PointeeSized + DynSend>(_t: &T) {}
pub fn assert_dyn_send_sync_val<T: ?Sized + PointeeSized + DynSync + DynSend>(_t: &T) {}

// A wrapper to convert a struct that is already a `Send` or `Sync` into
// an instance of `DynSend` and `DynSync`, since the compiler cannot infer
// it automatically in some cases. (e.g. Box<dyn Send / Sync>)
#[derive(Copy, Clone)]
pub struct IntoDynSyncSend<T: ?Sized + PointeeSized>(pub T);

unsafe impl<T: ?Sized + PointeeSized + Send> DynSend for IntoDynSyncSend<T> {}
unsafe impl<T: ?Sized + PointeeSized + Sync> DynSync for IntoDynSyncSend<T> {}

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
