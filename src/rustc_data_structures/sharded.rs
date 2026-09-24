// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::borrow::Borrow;
use core::hash::{Hash, Hasher};
use core::iter;
use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use either::Either;
use hashbrown::hash_table::{self, Entry, HashTable};

use crate::rustc_data_structures::fx::FxHasher;
use crate::rustc_data_structures::sync::{CacheAligned, Lock, LockGuard, Mode, is_dyn_thread_safe};

// 32 shards is sufficient to reduce contention on an 8-core Ryzen 7 1700,
// but this should be tested on higher core count CPUs. How the `Sharded` type gets used
// may also affect the ideal number of shards.
const SHARD_BITS: usize = 5;

const SHARDS: usize = 1 << SHARD_BITS;

/// An array of cache-line aligned inner locked structures with convenience methods.
/// A single field is used when the compiler uses only one thread.
pub enum Sharded<T> {
    Single(Lock<T>),
    Shards(Box<[CacheAligned<Lock<T>>; SHARDS]>),
}

impl<T: Default> Default for Sharded<T> {
    #[inline]
    fn default() -> Self {
        Self::new(T::default)
    }
}

impl<T> Sharded<T> {
    #[inline]
    pub fn new(mut value: impl FnMut() -> T) -> Self {
        if is_dyn_thread_safe() {
            return Sharded::Shards(Box::new(
                [(); SHARDS].map(|()| CacheAligned(Lock::new(value()))),
            ));
        }

        Sharded::Single(Lock::new(value()))
    }

    /// The shard is selected by hashing `val` with `FxHasher`.
    #[inline]
    pub fn get_shard_by_value<K: Hash + ?Sized>(&self, val: &K) -> &Lock<T> {
        match self {
            Self::Single(single) => single,
            Self::Shards(..) => self.get_shard_by_hash(make_hash(val)),
        }
    }

    #[inline]
    pub fn get_shard_by_hash(&self, hash: u64) -> &Lock<T> {
        self.get_shard_by_index(get_shard_hash(hash))
    }

    #[inline]
    pub fn get_shard_by_index(&self, i: usize) -> &Lock<T> {
        match self {
            Self::Single(single) => single,
            Self::Shards(shards) => {
                // SAFETY: The index gets ANDed with the shard mask, ensuring it is always inbounds.
                unsafe { &shards.get_unchecked(i & (SHARDS - 1)).0 }
            }
        }
    }

    /// The shard is selected by hashing `val` with `FxHasher`.
    #[inline]
    #[track_caller]
    pub fn lock_shard_by_value<K: Hash + ?Sized>(&self, val: &K) -> LockGuard<'_, T> {
        match self {
            // The lock's own recorded mode, not an assumed `NoSync`. See `lock_shard_by_index`.
            Self::Single(single) => single.lock(),
            Self::Shards(..) => self.lock_shard_by_hash(make_hash(val)),
        }
    }

    #[inline]
    #[track_caller]
    pub fn lock_shard_by_hash(&self, hash: u64) -> LockGuard<'_, T> {
        self.lock_shard_by_index(get_shard_hash(hash))
    }

    #[inline]
    #[track_caller]
    pub fn lock_shard_by_index(&self, i: usize) -> LockGuard<'_, T> {
        match self {
            // The lock's own recorded mode, not an assumed `NoSync`.
            //
            // `Single` is chosen when `is_dyn_thread_safe` is false, and `Lock::new` then picks
            // its kind from `might_be_dyn_thread_safe`, a second read. On a thread with a session
            // latched the two reads agree. On a thread without one they read the process-wide
            // fallback, which another thread can turn on between them (it only ever goes from
            // serial to thread-safe), and then the lock is the synchronised kind: assuming
            // `NoSync` would read the wrong half of its union. `lock` reads the kind the lock
            // recorded, at the cost of one branch.
            Self::Single(single) => single.lock(),
            Self::Shards(shards) => {
                // Synchronization is enabled so use the `lock_assume_sync` method optimized
                // for that case.

                // SAFETY (get_unchecked): The index gets ANDed with the shard mask, ensuring it is
                // always inbounds.
                // SAFETY (lock_assume_sync): We know `is_dyn_thread_safe` was true when creating
                // the lock thus `might_be_dyn_thread_safe` was also true.
                unsafe { shards.get_unchecked(i & (SHARDS - 1)).0.lock_assume(Mode::Sync) }
            }
        }
    }

    #[inline]
    pub fn lock_shards(&self) -> impl Iterator<Item = LockGuard<'_, T>> {
        match self {
            Self::Single(single) => Either::Left(iter::once(single.lock())),
            Self::Shards(shards) => Either::Right(shards.iter().map(|shard| shard.0.lock())),
        }
    }

    #[inline]
    pub fn try_lock_shards(&self) -> impl Iterator<Item = Option<LockGuard<'_, T>>> {
        match self {
            Self::Single(single) => Either::Left(iter::once(single.try_lock())),
            Self::Shards(shards) => Either::Right(shards.iter().map(|shard| shard.0.try_lock())),
        }
    }
}

#[inline]
pub fn shards() -> usize {
    if is_dyn_thread_safe() {
        return SHARDS;
    }

    1
}

pub type ShardedHashMap<K, V> = Sharded<hash_table::HashTable<(K, V)>>;

impl<K: Eq, V> ShardedHashMap<K, V> {
    pub fn with_capacity(cap: usize) -> Self {
        let per_shard_cap = cap.div_ceil(shards());
        Self::new(|| HashTable::with_capacity(per_shard_cap))
    }
    pub fn len(&self) -> usize {
        self.lock_shards().map(|shard| shard.len()).sum()
    }
}

impl<K: Eq + Hash, V> ShardedHashMap<K, V> {
    #[inline]
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
        V: Clone,
    {
        let hash = make_hash(key);
        let shard = self.lock_shard_by_hash(hash);
        let (_, value) = shard.find(hash, |(k, _)| k.borrow() == key)?;
        Some(value.clone())
    }

    #[inline]
    pub fn get_or_insert_with(&self, key: K, default: impl FnOnce() -> V) -> V
    where
        V: Copy,
    {
        let hash = make_hash(&key);
        let mut shard = self.lock_shard_by_hash(hash);

        match table_entry(&mut shard, hash, &key) {
            Entry::Occupied(e) => e.get().1,
            Entry::Vacant(e) => {
                let value = default();
                e.insert((key, value));
                value
            }
        }
    }

    /// Insert value into the [`ShardedHashMap`] with unique key.
    ///
    /// This function panics if debug_assertions are enabled and uniqueness is violated.
    /// If uniqueness is violated but debug_assertions are disabled then lookups will arbitrarily
    /// return one of the inserted elements.
    #[inline]
    pub fn insert_unique(&self, key: K, value: V) {
        let hash = make_hash(&key);
        let mut shard = self.lock_shard_by_hash(hash);

        cfg_select! {
            debug_assertions => match table_entry(&mut shard, hash, &key) {
                Entry::Occupied(_) => {
                    panic!("tried to insert key that's already present");
                }
                Entry::Vacant(e) => {
                    e.insert((key, value));
                }
            },
            _ => {
                shard.insert_unique(hash, (key, value), |(k, _)| make_hash(k));
            }
        }
    }
}

impl<K: Eq + Hash + Copy> ShardedHashMap<K, ()> {
    #[inline]
    pub fn intern_ref<Q: ?Sized>(&self, value: &Q, make: impl FnOnce() -> K) -> K
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = make_hash(value);
        let mut shard = self.lock_shard_by_hash(hash);

        match table_entry(&mut shard, hash, value) {
            Entry::Occupied(e) => e.get().0,
            Entry::Vacant(e) => {
                let v = make();
                e.insert((v, ()));
                v
            }
        }
    }

    #[inline]
    pub fn intern<Q>(&self, value: Q, make: impl FnOnce(Q) -> K) -> K
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = make_hash(&value);
        let mut shard = self.lock_shard_by_hash(hash);

        match table_entry(&mut shard, hash, &value) {
            Entry::Occupied(e) => e.get().0,
            Entry::Vacant(e) => {
                let v = make(value);
                e.insert((v, ()));
                v
            }
        }
    }
}

pub trait IntoPointer {
    /// Returns a pointer which outlives `self`.
    fn into_pointer(&self) -> *const ();
}

impl<K: Eq + Hash + Copy + IntoPointer> ShardedHashMap<K, ()> {
    pub fn contains_pointer_to<T: Hash + IntoPointer>(&self, value: &T) -> bool {
        let hash = make_hash(&value);
        let shard = self.lock_shard_by_hash(hash);
        let value = value.into_pointer();
        shard.find(hash, |(k, ())| k.into_pointer() == value).is_some()
    }
}

/// A key of an [`InternSet`]: a shared reference to a value that outlives the set, in all but
/// type. The set stores it as the pointer [`InternKey::into_raw`] gives and rebuilds it with
/// [`InternKey::from_raw`].
///
/// # Safety
///
/// `into_raw` returns a pointer that stays valid, and whose pointee never changes, for as long
/// as any set holding the key lives; `from_raw(k.into_raw())` is a key equal to `k` in every
/// respect (the same pointer, so the same `Hash`, `Eq` and `Borrow` results).
pub unsafe trait InternKey: Copy {
    fn into_raw(self) -> NonNull<()>;

    /// # Safety
    ///
    /// `raw` came from `into_raw` of a key of this type, and that key's pointee is still alive.
    unsafe fn from_raw(raw: NonNull<()>) -> Self;
}

/// The largest first table of an [`InternSet`] shard: 64 KiB of slots.
const INTERN_FIRST_MAX_SLOTS: usize = 4096;

/// One slot of an [`InternTable`]: empty while `key` is null, else the interned key and its
/// `make_hash`. Written once, under the shard's lock, and never changed again.
struct InternSlot {
    key: AtomicPtr<()>,
    hash: AtomicU64,
}

/// One open-addressed table of an [`InternSet`] shard, a power of two long.
struct InternTable {
    mask: usize,
    slots: Box<[InternSlot]>,
    /// The table this one replaced, kept alive (a reader may still be probing it) and freed
    /// with the whole chain when the set drops.
    prev: *mut InternTable,
}

impl InternTable {
    fn empty(len: usize, prev: *mut InternTable) -> InternTable {
        debug_assert!(len.is_power_of_two());
        // SAFETY: an all-zero `InternSlot` is a null `AtomicPtr` and a zero `AtomicU64`, the
        // empty slot.
        let slots = unsafe { Box::<[InternSlot]>::new_zeroed_slice(len).assume_init() };
        InternTable { mask: len - 1, slots, prev }
    }

    /// Probe from `slot` for a key `is_match` accepts among those of hash `hash`: the key, or
    /// the empty slot that ends the sequence. The table is at most half full, so one is reached.
    #[inline]
    fn probe<K: InternKey>(
        &self,
        hash: u64,
        mut slot: usize,
        is_match: impl Fn(K) -> bool,
    ) -> Result<K, usize> {
        loop {
            let s = &self.slots[slot];
            // `Acquire`: pairs with the `Release` that filled the slot, so its `hash` and the
            // key's pointee (written before it was interned) are visible.
            let Some(raw) = NonNull::new(s.key.load(Ordering::Acquire)) else {
                return Err(slot);
            };
            if s.hash.load(Ordering::Relaxed) == hash {
                // SAFETY: a published slot holds `into_raw` of a key of this set, whose pointee
                // outlives the set.
                let key = unsafe { K::from_raw(raw) };
                if is_match(key) {
                    return Ok(key);
                }
            }
            slot = (slot + 1) & self.mask;
        }
    }

    /// The first empty slot of `hash`'s probe sequence. Writer only.
    #[inline]
    fn vacant(&self, hash: u64) -> usize {
        let mut slot = hash as usize & self.mask;
        while !self.slots[slot].key.load(Ordering::Relaxed).is_null() {
            slot = (slot + 1) & self.mask;
        }
        slot
    }
}

/// Where a lock-free lookup in an [`InternSet`] shard stopped without finding its value: the
/// table it probed and the empty slot it reached. Only compared, never dereferenced.
#[derive(Clone, Copy)]
struct InternMiss {
    table: *const InternTable,
    slot: usize,
}

/// A set of interned values, keyed by content: looking up a value already in it takes no
/// lock, and only inserting one does.
///
/// The type interners hit the same hot values from every worker of a parallel session, and with
/// a lock per lookup the shard's lock line moved between cores on every call. This keeps the
/// `Sharded` layout for writers (one lock per shard, the shard picked by the hash's high bits
/// as before) but gives each shard an open-addressed table of published keys, which readers
/// probe with `Acquire` loads and compare by value (the stored hash first, then `Borrow` and
/// `Eq`, exactly as the hash table this replaces did). It is `rustc_span::symbol`'s
/// `SymbolIndices` discipline, with a key pointer and its full hash in place of a packed index:
///
/// - An insert fills the slot's `hash`, then stores its key with `Release`. A reader that loads
///   the key with `Acquire` and sees it non-null sees the hash and the key's pointee.
/// - Growth builds a whole new table from the current one under the lock, then publishes its
///   pointer with `Release`; a reader loads the pointer with `Acquire` and sees every slot the
///   builder wrote. The old table is never written again and stays allocated until the set
///   drops, so a reader still probing it reads valid, frozen memory. It can only miss values
///   inserted after the switch, and a miss goes to the locked path, which probes the current
///   table.
/// - Writers of a shard are serialised by its lock, whose acquire makes every earlier writer's
///   stores visible, so the probe under the lock sees every value inserted so far. Equal values
///   have equal hashes and so the same shard: a value is inserted at most once, and every
///   intern of it returns the same key, however lookups and inserts interleave.
/// - A slot is written once and never cleared, so if the table a lock-free lookup missed in is
///   still current, every slot before the empty one it stopped at is unchanged, and the value,
///   if another writer has inserted it since, is at or after that slot. The locked insert
///   resumes there instead of probing the sequence again.
///
/// A serial session has one shard and a lock that does not synchronise; the lookup is the same
/// probe, without a lock at all.
pub struct InternSet<K> {
    /// The current table of each shard, null until its first insert. Written only on growth,
    /// so readers share these lines without the writers' lock traffic.
    tables: InternTables,
    /// Each shard's writer lock, over the number of keys in its current table.
    locks: Box<[CacheAligned<Lock<usize>>]>,
    /// The length of a shard's first table.
    first_len: usize,
    marker: PhantomData<K>,
}

impl<K: InternKey> InternSet<K> {
    pub fn with_capacity(cap: usize) -> Self {
        let n = shards();
        let per_shard_cap = cap.div_ceil(n);
        // At most half full once `per_shard_cap` keys are in, but no more than
        // `INTERN_FIRST_MAX_SLOTS`: a session is often one file, whose keys would scatter over
        // every page of a table sized for a whole crate. Growth only copies (the hash is kept).
        let first_len = per_shard_cap
            .saturating_mul(2)
            .clamp(16, INTERN_FIRST_MAX_SLOTS)
            .next_power_of_two();
        InternSet {
            tables: InternTables((0..n).map(|_| AtomicPtr::new(ptr::null_mut())).collect()),
            locks: (0..n).map(|_| CacheAligned(Lock::new(0))).collect(),
            first_len,
            marker: PhantomData,
        }
    }

    #[inline]
    fn shard_of(&self, hash: u64) -> usize {
        // `tables.len()` is 1 or `SHARDS`, both powers of two.
        get_shard_hash(hash) & (self.tables.len() - 1)
    }

    /// The key of `hash` that `is_match` accepts, without any lock.
    #[inline]
    fn find(&self, shard: usize, hash: u64, is_match: impl Fn(K) -> bool) -> Result<K, InternMiss> {
        let table = self.tables[shard].load(Ordering::Acquire);
        if table.is_null() {
            return Err(InternMiss { table, slot: 0 });
        }
        // SAFETY: a published table is fully built (the `Acquire` above pairs with the
        // `Release` that published it) and lives until `self` drops.
        let t = unsafe { &*table };
        t.probe(hash, hash as usize & t.mask, is_match).map_err(|slot| InternMiss { table, slot })
    }

    /// Under `shard`'s lock (the caller holds it): the key `is_match` accepts, probing the
    /// current table from where the lock-free `miss` stopped when that table is still current.
    #[inline]
    fn find_locked(
        &self,
        shard: usize,
        hash: u64,
        miss: InternMiss,
        is_match: impl Fn(K) -> bool,
    ) -> Option<K> {
        let table = self.tables[shard].load(Ordering::Acquire);
        if table.is_null() {
            return None;
        }
        // SAFETY: published, alive until `self` drops (see `find`).
        let t = unsafe { &*table };
        let from = if ptr::eq(table, miss.table) { miss.slot } else { hash as usize & t.mask };
        t.probe(hash, from, is_match).ok()
    }

    /// Insert `key` of hash `hash` into `shard`, whose lock the caller holds as `count` (the
    /// number of keys in the shard), and whose current table does not hold an equal key.
    fn publish(&self, shard: usize, count: &mut usize, hash: u64, key: K) {
        let mut table = self.tables[shard].load(Ordering::Acquire);
        // SAFETY: a non-null current table is published and alive until `self` drops.
        let len = if table.is_null() { 0 } else { unsafe { (&*table).slots.len() } };
        if (*count + 1) * 2 > len {
            table = self.grow(shard, table, len);
        }
        // SAFETY: the current table, alive until `self` drops.
        let t = unsafe { &*table };
        let slot = &t.slots[t.vacant(hash)];
        slot.hash.store(hash, Ordering::Relaxed);
        // `Release`: whoever sees the key sees its hash, and the pointee `make` wrote.
        slot.key.store(key.into_raw().as_ptr(), Ordering::Release);
        *count += 1;
    }

    /// Replace `shard`'s current table `old` (of `old_len` slots, or null) with one twice as
    /// large holding the same keys, publish it, and return it. Writer only.
    #[cold]
    #[inline(never)]
    fn grow(&self, shard: usize, old: *mut InternTable, old_len: usize) -> *mut InternTable {
        let len = if old_len == 0 { self.first_len } else { old_len * 2 };
        let fresh = InternTable::empty(len, old);
        if !old.is_null() {
            // SAFETY: the current table, alive until `self` drops.
            let old = unsafe { &*old };
            for s in old.slots.iter() {
                let key = s.key.load(Ordering::Relaxed);
                if key.is_null() {
                    continue;
                }
                let hash = s.hash.load(Ordering::Relaxed);
                let to = &fresh.slots[fresh.vacant(hash)];
                // Unpublished: plain stores, made visible by the `Release` below.
                to.hash.store(hash, Ordering::Relaxed);
                to.key.store(key, Ordering::Relaxed);
            }
        }
        let fresh = Box::into_raw(Box::new(fresh));
        self.tables[shard].store(fresh, Ordering::Release);
        fresh
    }

    /// The key equal to `value`, making and inserting it with `make` when absent.
    #[inline]
    pub fn intern<Q>(&self, value: Q, make: impl FnOnce(Q) -> K) -> K
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = make_hash(&value);
        let shard = self.shard_of(hash);
        let miss = match self.find(shard, hash, |k: K| <K as Borrow<Q>>::borrow(&k) == &value) {
            Ok(key) => return key,
            Err(miss) => miss,
        };
        let mut count = self.locks[shard].0.lock();
        let is_match = |k: K| <K as Borrow<Q>>::borrow(&k) == &value;
        if let Some(key) = self.find_locked(shard, hash, miss, is_match) {
            return key;
        }
        let key = make(value);
        self.publish(shard, &mut count, hash, key);
        key
    }

    /// The key equal to `*value`, making and inserting it with `make` when absent.
    #[inline]
    pub fn intern_ref<Q: ?Sized>(&self, value: &Q, make: impl FnOnce() -> K) -> K
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = make_hash(value);
        let shard = self.shard_of(hash);
        let is_match = |k: K| <K as Borrow<Q>>::borrow(&k) == value;
        let miss = match self.find(shard, hash, is_match) {
            Ok(key) => return key,
            Err(miss) => miss,
        };
        let mut count = self.locks[shard].0.lock();
        if let Some(key) = self.find_locked(shard, hash, miss, is_match) {
            return key;
        }
        let key = make();
        self.publish(shard, &mut count, hash, key);
        key
    }

    /// Whether the set holds the very pointer `value` is (not merely an equal value).
    pub fn contains_pointer_to<T: Hash + IntoPointer>(&self, value: &T) -> bool {
        let hash = make_hash(value);
        let shard = self.shard_of(hash);
        let target = value.into_pointer();
        let is_match = |k: K| ptr::eq(k.into_raw().as_ptr() as *const (), target);
        match self.find(shard, hash, is_match) {
            Ok(_) => true,
            Err(miss) => {
                let _count = self.locks[shard].0.lock();
                self.find_locked(shard, hash, miss, is_match).is_some()
            }
        }
    }

    /// The number of keys, counted under every shard's lock.
    pub fn len(&self) -> usize {
        self.locks.iter().map(|lock| *lock.0.lock()).sum()
    }

    /// Call `f` on every key, one shard at a time under its lock (so `f` must not intern into
    /// this set), in no particular order.
    pub fn for_each(&self, mut f: impl FnMut(K)) {
        for (shard, lock) in self.locks.iter().enumerate() {
            let _count = lock.0.lock();
            let table = self.tables[shard].load(Ordering::Acquire);
            if table.is_null() {
                continue;
            }
            // SAFETY: published, alive until `self` drops.
            for s in unsafe { &*table }.slots.iter() {
                if let Some(raw) = NonNull::new(s.key.load(Ordering::Acquire)) {
                    // SAFETY: a published slot holds `into_raw` of a key of this set.
                    f(unsafe { K::from_raw(raw) });
                }
            }
        }
    }
}

/// The current table of each shard of an [`InternSet`]. Not generic, so that freeing the tables
/// puts no drop-check requirement on the set's keys, which it only borrows.
struct InternTables(Box<[AtomicPtr<InternTable>]>);

impl core::ops::Deref for InternTables {
    type Target = [AtomicPtr<InternTable>];

    #[inline]
    fn deref(&self) -> &[AtomicPtr<InternTable>] {
        &self.0
    }
}

impl Drop for InternTables {
    fn drop(&mut self) {
        for table in self.0.iter_mut() {
            let mut table = *table.get_mut();
            while !table.is_null() {
                // SAFETY: every table was made by `Box::into_raw` in `grow` and is reachable
                // once: the current one from `tables`, each older one from its successor's
                // `prev`. The keys are borrowed, not owned, so nothing else is dropped.
                let owned = unsafe { Box::from_raw(table) };
                table = owned.prev;
            }
        }
    }
}

#[inline]
pub fn make_hash<K: Hash + ?Sized>(val: &K) -> u64 {
    let mut state = FxHasher::default();
    val.hash(&mut state);
    state.finish()
}

#[inline]
fn table_entry<'a, K, V, Q>(
    table: &'a mut HashTable<(K, V)>,
    hash: u64,
    key: &Q,
) -> Entry<'a, (K, V)>
where
    K: Hash + Borrow<Q>,
    Q: ?Sized + Eq,
{
    table.entry(hash, move |(k, _)| k.borrow() == key, |(k, _)| make_hash(k))
}

/// Get a shard with a pre-computed hash value. If `get_shard_by_value` is
/// ever used in combination with `get_shard_by_hash` on a single `Sharded`
/// instance, then `hash` must be computed with `FxHasher`. Otherwise,
/// `hash` can be computed with any hasher, so long as that hasher is used
/// consistently for each `Sharded` instance.
#[inline]
fn get_shard_hash(hash: u64) -> usize {
    let hash_len = size_of::<usize>();
    // Ignore the top 7 bits as hashbrown uses these and get the next SHARD_BITS highest bits.
    // hashbrown also uses the lowest bits, so we can't use those
    (hash >> (hash_len * 8 - 7 - SHARD_BITS)) as usize
}
