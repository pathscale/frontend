// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::num::NonZero;
use core::ops::Deref;
use core::ptr;
use alloc::sync::Arc;

use parking_lot::Mutex;

use eko::thread::ThreadLocal;

use crate::rustc_data_structures::outline;
use crate::rustc_data_structures::sync::CacheAligned;

// # A registry per session, and slots leased to threads
//
// Upstream a `Registry` was rayon's thread pool seen from `WorkerLocal`: every pool thread was
// registered once, for good, and its index was its rayon thread index. Here the threads belong to
// nagoya and serve any session that asks, one item at a time, so a thread's registry is not a
// property of the thread but of the item it is running:
//
// - `run_compiler` makes one `Registry` per session, with one slot per thread the session may
//   use (`jobs.frontend`, or one), and leases slot 0 to its own thread for the whole session.
// - A pool helper leases a free slot when it starts serving that session and gives it back when
//   it leaves. Its `WorkerLocal` values are the slot's, whichever OS thread holds it.
// - A thread enters a registry for the duration of a scope and leaves it on the way out, so a
//   worker that served one session and then another is never left pointing at the first.
//
// A slot is held by one thread at a time, so a `WorkerLocal` value is still used by one thread at
// a time, which is all `WorkerLocal` promises. The slot count is also the session's thread
// budget: a helper that finds no slot free leaves the work to the threads that have one.
//
// This replaced a registry that survived across requests on each thread, keyed to the first
// session's thread count. That was a cache of the first session's shape, and with sessions
// sharing pool threads it was also wrong: a later session's arenas were sized by an earlier
// session's `jobs.frontend`.

/// A pointer to the `RegistryData` which uniquely identifies a registry.
/// This identifier can be reused if the registry gets freed.
#[derive(Clone, Copy, PartialEq)]
struct RegistryId(*const RegistryData);

impl RegistryId {
    #[inline(always)]
    /// Verifies that the current thread is associated with the registry and returns its unique
    /// index within the registry. This panics if the current thread is not associated with this
    /// registry.
    ///
    /// Note that there's a race possible where the identifier in `THREAD_DATA` could be reused
    /// so this can succeed from a different registry.
    fn verify(self) -> usize {
        let (id, index) = THREAD_DATA
            .with(ThreadData::new, |data| (data.registry_id, data.index))
            .expect("out of thread-local slots");

        if id == self { index } else { outline(|| panic!("Unable to verify registry association")) }
    }
}

struct RegistryData {
    thread_limit: NonZero<usize>,
    /// Slot indices not currently held by any thread.
    free: Mutex<Vec<usize>>,
}

/// Represents a list of threads which can access worker locals.
#[derive(Clone)]
pub struct Registry(Arc<RegistryData>);

/// The registry the thread is in, and its slot index in it.
///
/// `eko::thread::ThreadLocal` rather than `std::thread_local!`: the macro comes from the standard
/// prelude and names no path, so it linked `std` while reading clean. `with` answers `None` when
/// the process is out of `pthread` key slots; the sites below treat that as unreachable, which is
/// what the macro's own `with` did by panicking.
static REGISTRY: ThreadLocal<Option<Registry>> = ThreadLocal::new();

#[derive(Clone, Copy)]
struct ThreadData {
    registry_id: RegistryId,
    index: usize,
}

impl ThreadData {
    /// What a thread starts with: in no registry.
    fn new() -> ThreadData {
        ThreadData { registry_id: RegistryId(ptr::null()), index: 0 }
    }
}

/// A thread local which contains the identifier of `REGISTRY` but allows for faster access.
/// It also holds the index of the current thread.
static THREAD_DATA: ThreadLocal<ThreadData> = ThreadLocal::new();

impl Registry {
    /// Creates a registry which can hold up to `thread_limit` threads.
    pub fn new(thread_limit: NonZero<usize>) -> Self {
        // Reversed, so `pop` hands out 0 first: the session's own thread gets slot 0, as the
        // main thread had index 0 upstream.
        let free = (0..thread_limit.get()).rev().collect();
        Registry(Arc::new(RegistryData { thread_limit, free: Mutex::new(free) }))
    }

    /// Gets the registry associated with the current thread. Panics if there's no such registry.
    pub fn current() -> Self {
        Self::try_current().expect("No associated registry")
    }

    /// Gets the registry the current thread is in, if any.
    pub fn try_current() -> Option<Self> {
        REGISTRY.with(|| None, |registry| registry.clone()).expect("out of thread-local slots")
    }

    /// Registers the current thread with the registry for good, so worker locals can be used on
    /// it. Panics if the thread limit is hit or if the thread already has an associated registry.
    ///
    /// Kept for a caller that owns a thread for one registry's whole life. The compiler itself
    /// leases slots instead: see [`Registry::lease`].
    pub fn register(&self) {
        if Self::try_current().is_some() {
            panic!("Thread already has a registry");
        }
        let Some(slot) = self.lease() else { panic!("Thread limit reached") };
        let index = slot.index;
        // Never given back, and never left: the thread keeps the slot for as long as it lives.
        core::mem::forget(slot);
        core::mem::forget(enter_raw(Some(self.clone()), ThreadData { registry_id: self.id(), index }));
    }

    /// Take a free slot, or `None` if every slot is held.
    pub fn lease(&self) -> Option<RegistrySlot> {
        let index = self.0.free.lock().pop()?;
        Some(RegistrySlot { registry: self.clone(), index })
    }

    /// Run `op` once per slot, on this thread, with this thread in that slot for the call, and
    /// collect the results in slot order.
    ///
    /// Upstream this was rayon's `broadcast`: `op` ran on every pool thread, each reading its own
    /// `WorkerLocal` values. Those values belong to slots here, not to threads, so visiting every
    /// slot from one thread reads exactly the same values, with no thread to wake.
    ///
    /// # Panics
    ///
    /// If any slot but this thread's own is leased: another thread could be using its values.
    /// Call it between parallel stages, as its one caller (the dep-graph encoder's finish) does.
    pub fn each_slot<R>(&self, mut op: impl FnMut(usize) -> R) -> Vec<R> {
        let own = THREAD_DATA
            .with(ThreadData::new, |data| (data.registry_id == self.id()).then_some(data.index))
            .flatten();
        let limit = self.0.thread_limit.get();
        {
            let free = self.0.free.lock();
            let held = limit - free.len();
            assert!(
                held <= usize::from(own.is_some()),
                "a WorkerLocal broadcast while another thread holds one of its slots"
            );
        }
        (0..limit)
            .map(|index| {
                let _scope =
                    enter_raw(Some(self.clone()), ThreadData { registry_id: self.id(), index });
                op(index)
            })
            .collect()
    }

    /// Gets the identifier of this registry.
    fn id(&self) -> RegistryId {
        RegistryId(&*self.0)
    }
}

/// One slot of a registry, held by one thread at a time. Given back when dropped.
pub struct RegistrySlot {
    registry: Registry,
    index: usize,
}

impl RegistrySlot {
    /// Put the current thread in this slot until the scope drops, then back where it was.
    pub fn enter(&self) -> RegistryScope {
        enter_raw(
            Some(self.registry.clone()),
            ThreadData { registry_id: self.registry.id(), index: self.index },
        )
    }
}

impl Drop for RegistrySlot {
    fn drop(&mut self) {
        self.registry.0.free.lock().push(self.index);
    }
}

/// The current thread's previous registry and slot, put back when this drops.
#[must_use = "the thread leaves the registry again when this drops"]
pub struct RegistryScope {
    registry: Option<Registry>,
    data: ThreadData,
}

fn enter_raw(registry: Option<Registry>, data: ThreadData) -> RegistryScope {
    let registry = REGISTRY
        .with(|| None, |slot| core::mem::replace(slot, registry))
        .expect("out of thread-local slots");
    let data = THREAD_DATA
        .with(ThreadData::new, |slot| core::mem::replace(slot, data))
        .expect("out of thread-local slots");
    RegistryScope { registry, data }
}

impl Drop for RegistryScope {
    fn drop(&mut self) {
        let _ = THREAD_DATA.with(ThreadData::new, |slot| *slot = self.data);
        let previous = self.registry.take();
        // The displaced registry is dropped outside the slot: nothing runs while it is borrowed.
        let displaced = REGISTRY.with(|| None, |slot| core::mem::replace(slot, previous));
        drop(displaced);
    }
}

/// Holds worker local values for each possible thread in a registry. You can only access the
/// worker local value through the `Deref` impl on the registry associated with the thread it was
/// created on. It will panic otherwise.
pub struct WorkerLocal<T> {
    locals: Box<[CacheAligned<T>]>,
    registry: Registry,
}

// This is safe because the `deref` call will return a reference to a `T` unique to each slot,
// and a slot is held by one thread at a time (see `RegistrySlot`), or it will panic for threads
// without an associated local. So there isn't a need for `T` to do
// it's own synchronization. The `verify` method on `RegistryId` has an issue where the id
// can be reused, but `WorkerLocal` has a reference to `Registry` which will prevent any reuse.
unsafe impl<T: Send> Sync for WorkerLocal<T> {}

impl<T> WorkerLocal<T> {
    /// Creates a new worker local where the `initial` closure computes the
    /// value this worker local should take for each thread in the registry.
    #[inline]
    pub fn new<F: FnMut(usize) -> T>(mut initial: F) -> WorkerLocal<T> {
        let registry = Registry::current();
        WorkerLocal {
            locals: (0..registry.0.thread_limit.get()).map(|i| CacheAligned(initial(i))).collect(),
            registry,
        }
    }

    /// Returns the worker-local values for each thread
    #[inline]
    pub fn into_inner(self) -> impl Iterator<Item = T> {
        self.locals.into_vec().into_iter().map(|local| local.0)
    }
}

impl<T> Deref for WorkerLocal<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        // This is safe because `verify` will only return values less than
        // `self.registry.thread_limit` which is the size of the `self.locals` array.
        unsafe { &self.locals.get_unchecked(self.registry.id().verify()).0 }
    }
}

impl<T: Default> Default for WorkerLocal<T> {
    fn default() -> Self {
        WorkerLocal::new(|_| T::default())
    }
}
