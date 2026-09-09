// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::cell::{Cell, OnceCell};
use core::num::NonZero;
use core::ops::Deref;
use core::ptr;
use alloc::sync::Arc;

use parking_lot::Mutex;

use eko::thread::ThreadLocal;

use crate::rustc_data_structures::outline;
use crate::rustc_data_structures::sync::CacheAligned;

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
            .with(ThreadData::new, |data| (data.registry_id.get(), data.index.get()))
            .expect("out of thread-local slots");

        if id == self { index } else { outline(|| panic!("Unable to verify registry association")) }
    }
}

struct RegistryData {
    thread_limit: NonZero<usize>,
    threads: Mutex<usize>,
}

/// Represents a list of threads which can access worker locals.
#[derive(Clone)]
pub struct Registry(Arc<RegistryData>);

/// The registry associated with the thread.
/// This allows the `WorkerLocal` type to clone the registry in its constructor.
///
/// `eko::thread::ThreadLocal` rather than `std::thread_local!`: the macro comes from
/// the standard prelude and names no path, so it linked `std` while reading clean. The
/// initialiser that was a `const { .. }` block is now the first argument to `with`, and `with`
/// answers `None` when the process is out of `pthread` key slots - the sites below treat that
/// as unreachable, which is what the macro's own `with` did by panicking.
static REGISTRY: ThreadLocal<OnceCell<Registry>> = ThreadLocal::new();

struct ThreadData {
    registry_id: Cell<RegistryId>,
    index: Cell<usize>,
}

impl ThreadData {
    /// What a thread starts with. This was the `const { .. }` initialiser of the
    /// `thread_local!` block.
    fn new() -> ThreadData {
        ThreadData { registry_id: Cell::new(RegistryId(ptr::null())), index: Cell::new(0) }
    }
}

/// A thread local which contains the identifier of `REGISTRY` but allows for faster access.
/// It also holds the index of the current thread.
static THREAD_DATA: ThreadLocal<ThreadData> = ThreadLocal::new();

impl Registry {
    /// Creates a registry which can hold up to `thread_limit` threads.
    pub fn new(thread_limit: NonZero<usize>) -> Self {
        Registry(Arc::new(RegistryData { thread_limit, threads: Mutex::new(0) }))
    }

    /// Gets the registry associated with the current thread. Panics if there's no such registry.
    pub fn current() -> Self {
        REGISTRY
            .with(OnceCell::new, |registry| registry.get().cloned())
            .expect("out of thread-local slots")
            .expect("No associated registry")
    }

    /// Gets the registry already associated with this thread, if any.
    ///
    /// A batch compiler normally registers a fresh worker thread for every invocation. This compiler's
    /// frontend runs serial invocations on one thread, so its
    /// registry deliberately survives the request-scoped compiler state.
    pub fn try_current() -> Option<Self> {
        REGISTRY.with(OnceCell::new, |registry| registry.get().cloned()).flatten()
    }

    /// Registers the current thread with the registry so worker locals can be used on it.
    /// Panics if the thread limit is hit or if the thread already has an associated registry.
    pub fn register(&self) {
        let mut threads = self.0.threads.lock();
        if *threads < self.0.thread_limit.get() {
            REGISTRY
                .with(OnceCell::new, |registry| {
                    if registry.get().is_some() {
                        drop(threads);
                        panic!("Thread already has a registry");
                    }
                    registry.set(self.clone()).ok();
                    THREAD_DATA
                        .with(ThreadData::new, |data| {
                            data.registry_id.set(self.id());
                            data.index.set(*threads);
                        })
                        .expect("out of thread-local slots");
                    *threads += 1;
                })
                .expect("out of thread-local slots");
        } else {
            drop(threads);
            panic!("Thread limit reached");
        }
    }

    /// Gets the identifier of this registry.
    fn id(&self) -> RegistryId {
        RegistryId(&*self.0)
    }
}

/// Holds worker local values for each possible thread in a registry. You can only access the
/// worker local value through the `Deref` impl on the registry associated with the thread it was
/// created on. It will panic otherwise.
pub struct WorkerLocal<T> {
    locals: Box<[CacheAligned<T>]>,
    registry: Registry,
}

// This is safe because the `deref` call will return a reference to a `T` unique to each thread
// or it will panic for threads without an associated local. So there isn't a need for `T` to do
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
