// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::hash::{BuildHasher, Hash, Hasher};
use core::marker::PhantomData;
use core::mem;
use core::num::NonZero;

use crate::rustc_index::bit_set::{self, DenseBitSet};
use crate::rustc_index::{Idx, IndexSlice, IndexVec};
use smallvec::SmallVec;
use thin_vec::ThinVec;

use crate::rustc_data_structures::fingerprint::Fingerprint;

#[cfg(test)]
mod tests;

use crate::rustc_hashes::{Hash64, Hash128};
pub use crate::rustc_stable_hash::{
    FromStableHash, SipHasher128Hash as StableHasherHash, StableSipHasher128 as StableHasher,
};

/// This trait lets `StableHash` and `derive(StableHash)` be used in
/// this crate (and other crates upstream of `rustc_middle`), while leaving
/// certain operations to be defined in `rustc_middle` where more things are
/// visible.
pub trait StableHashCtxt {
    /// The main event: stable hashing of a span.
    fn stable_hash_span(&mut self, span: RawSpan, hasher: &mut StableHasher);

    /// Compute a `Fingerprint`, which can be trivially turned into a `DefPathHash`.
    fn def_path_hash(&self, def_id: RawDefId) -> Fingerprint;

    /// Get the stable hash controls.
    fn stable_hash_controls(&self) -> StableHashControls;

    /// Assert that the provided `StableHashCtxt` is configured with the default
    /// `StableHashControls`. We should always have bailed out before getting to here with a
    fn assert_default_stable_hash_controls(&self, msg: &str);

    /// The fingerprint already computed, *in this hashing computation*, for the interned value
    /// at `addr`, under the current `StableHashControls`.
    ///
    /// # Why this is on the context and not in a thread-local
    ///
    /// `&'tcx RawList<H, T>` and `AdtDefData` are interned, so one address is one value for as
    /// long as the arena that handed it out lives, and an interned type graph is a DAG that
    /// shares sub-lists freely. Hashing it without a memo re-hashes every shared node once per
    /// path to it, which is exponential in nesting depth (upstream measured ~4,000x on
    /// `deeply-nested-multi`).
    ///
    /// The memo used to be a `thread_local!` per `(H, T)` and per thread, living as long as the
    /// thread. That made it outlive the arenas its keys pointed into, which took a process-wide
    /// `ADDRESS_CACHE_GENERATION` counter bumped per `GlobalCtxt` to paper over, and with
    /// sessions sharing nagoya workers even the counter was not enough (a worker could hash a
    /// live session's value at an address a dead session's entry still named, with no new
    /// context created in between). It was also shared mutable state that every parallel stage
    /// touched.
    ///
    /// Owned by the hashing context instead, the memo lives exactly as long as one
    /// `with_stable_hashing_context` call, during which every address it holds is alive. It
    /// still removes the exponential re-hashing inside that call, which is what the memo was
    /// for; it no longer carries results from one call to the next, which was a single-thread
    /// speed-up the owner asked to drop. Nothing to invalidate, nothing shared, nothing global.
    ///
    /// The default is no memo, which is always correct: the value is hashed again.
    #[inline]
    fn memoized_address_hash(&self, addr: usize) -> Option<Fingerprint> {
        let _ = addr;
        None
    }

    /// Records `hash` as the fingerprint of the interned value at `addr` under the current
    /// `StableHashControls`, for the rest of this hashing computation. See
    /// [`StableHashCtxt::memoized_address_hash`].
    #[inline]
    fn memoize_address_hash(&mut self, addr: usize, hash: Fingerprint) {
        let _ = (addr, hash);
    }
}

// A type used to work around `Span` not being visible in this crate. It is the same layout as
// `Span`.
pub struct RawSpan(pub u32, pub u16, pub u16);

// A type used to work around `DefId` not being visible in this crate. It is the same size as
// `DefId`.
pub struct RawDefId(pub u32, pub u32);

/// Something that implements `StableHash` can be hashed in a way that is
/// stable across multiple compilation sessions.
///
/// Note that `StableHash` imposes rather more strict requirements than usual
/// hash functions:
///
/// - Stable hashes are sometimes used as identifiers. Therefore they must
///   conform to the corresponding `PartialEq` implementations:
///
///     - `x == y` implies `stable_hash(x) == stable_hash(y)`, and
///     - `x != y` implies `stable_hash(x) != stable_hash(y)`.
///
///   That second condition is usually not required for hash functions
///   (e.g. `Hash`). In practice this means that `stable_hash` must feed any
///   information into the hasher that a `PartialEq` comparison takes into
///   account. See [#49300](https://github.com/rust-lang/rust/issues/49300)
///   for an example where violating this invariant has caused trouble in the
///   past.
///
/// - `stable_hash()` must be independent of the current
///    compilation session. E.g. they must not hash memory addresses or other
///    things that are "randomly" assigned per compilation session.
///
/// - `stable_hash()` must be independent of the host architecture. The
///   `StableHasher` takes care of endianness and `isize`/`usize` platform
///   differences.
pub trait StableHash {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher);

    /// Hashes the elements of a slice, after `[T]` has hashed the length.
    ///
    /// This is the `Hash::hash_slice` pattern. It replaces a specialized `[u8]` impl
    /// (stable Rust cannot specialize): `u8` overrides it to write the bytes in one call.
    #[inline]
    fn stable_hash_slice<Hcx: StableHashCtxt>(
        slice: &[Self],
        hcx: &mut Hcx,
        hasher: &mut StableHasher,
    ) where
        Self: Sized,
    {
        for item in slice {
            item.stable_hash(hcx, hasher);
        }
    }
}

/// Implement this for types that can be turned into stable keys like, for
/// example, for DefId that can be converted to a DefPathHash. This is used for
/// bringing maps into a predictable order before hashing them.
pub trait ToStableHashKey {
    type KeyType: Ord + Sized + StableHash;
    fn to_stable_hash_key<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx) -> Self::KeyType;
}

/// Trait for marking a type as having a sort order that is
/// stable across compilation session boundaries. More formally:
///
/// ```txt
/// Ord::cmp(a1, b1) == Ord::cmp(a2, b2)
///    where a2 = decode(encode(a1, context1), context2)
///          b2 = decode(encode(b1, context1), context2)
/// ```
///
/// i.e. the result of `Ord::cmp` is not influenced by encoding
/// the values in one session and then decoding them in another
/// session.
///
/// This is trivially true for types where encoding and decoding
/// don't change the bytes of the values that are used during
/// comparison and comparison only depends on these bytes (as
/// opposed to some non-local state). Examples are u32, String,
/// Path, etc.
///
/// But it is not true for:
///  - `*const T` and `*mut T` because the values of these pointers
///    will change between sessions.
///  - `DefIndex`, `CrateNum`, `LocalDefId`, because their concrete
///    values depend on state that might be different between
///    compilation sessions.
///
/// The associated constant `CAN_USE_UNSTABLE_SORT` denotes whether
/// unstable sorting can be used for this type. Set to true if and
/// only if `a == b` implies `a` and `b` are fully indistinguishable.
pub trait StableOrd: Ord {
    const CAN_USE_UNSTABLE_SORT: bool;

    /// Marker to ensure that implementors have carefully considered
    /// whether their `Ord` implementation obeys this trait's contract.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: ();
}

impl<T: StableOrd> StableOrd for &T {
    const CAN_USE_UNSTABLE_SORT: bool = T::CAN_USE_UNSTABLE_SORT;

    // Ordering of a reference is exactly that of the referent, and since
    // the ordering of the referet is stable so must be the ordering of the
    // reference.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

/// This is a companion trait to `StableOrd`. Some types like `Symbol` can be
/// compared in a cross-session stable way, but their `Ord` implementation is
/// not stable. In such cases, a `StableOrd` implementation can be provided
/// to offer a lightweight way for stable sorting. (The more heavyweight option
/// is to sort via `ToStableHashKey`, but then sorting needs to have access to
/// a stable hashing context and `ToStableHashKey` can also be expensive as in
/// the case of `Symbol` where it has to allocate a `String`.)
///
/// See the documentation of [StableOrd] for how stable sort order is defined.
/// The same definition applies here. Be careful when implementing this trait.
pub trait StableCompare {
    const CAN_USE_UNSTABLE_SORT: bool;

    fn stable_cmp(&self, other: &Self) -> core::cmp::Ordering;
}

/// `StableOrd` denotes that the type's `Ord` implementation is stable, so
/// we can implement `StableCompare` by just delegating to `Ord`.
impl<T: StableOrd> StableCompare for T {
    const CAN_USE_UNSTABLE_SORT: bool = T::CAN_USE_UNSTABLE_SORT;

    fn stable_cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.cmp(other)
    }
}

/// Implement StableHash by just calling `Hash::hash()`. Also implement `StableOrd` for the type
/// since that has the same requirements.
///
/// **WARNING** This is only valid for types that *really* don't need any context for fingerprinting.
/// But it is easy to misuse this macro (see [#96013](https://github.com/rust-lang/rust/issues/96013)
/// for examples). Therefore this macro is not exported and should only be used in the limited cases
/// here in this module.
///
/// Use `#[derive(StableHash)]` instead.
macro_rules! impl_stable_traits_for_trivial_type {
    ($t:ty) => {
        impl $crate::rustc_data_structures::stable_hash::StableHash for $t {
            #[inline]
            fn stable_hash<Hcx>(
                &self,
                _: &mut Hcx,
                hasher: &mut $crate::rustc_data_structures::stable_hash::StableHasher,
            ) {
                ::core::hash::Hash::hash(self, hasher);
            }
        }

        impl $crate::rustc_data_structures::stable_hash::StableOrd for $t {
            const CAN_USE_UNSTABLE_SORT: bool = true;

            // Encoding and decoding doesn't change the bytes of trivial types
            // and `Ord::cmp` depends only on those bytes.
            const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
        }
    };
}

pub(crate) use impl_stable_traits_for_trivial_type;

impl_stable_traits_for_trivial_type!(i8);
impl_stable_traits_for_trivial_type!(i16);
impl_stable_traits_for_trivial_type!(i32);
impl_stable_traits_for_trivial_type!(i64);
impl_stable_traits_for_trivial_type!(isize);

// `u8` is spelled out rather than macro-generated so it can override `stable_hash_slice`:
// a byte slice goes to the hasher in one `write`, as the old specialized `[u8]` impl did.
impl StableHash for u8 {
    #[inline]
    fn stable_hash<Hcx>(&self, _: &mut Hcx, hasher: &mut StableHasher) {
        ::core::hash::Hash::hash(self, hasher);
    }

    #[inline]
    fn stable_hash_slice<Hcx: StableHashCtxt>(
        slice: &[Self],
        _: &mut Hcx,
        hasher: &mut StableHasher,
    ) {
        hasher.write(slice);
    }
}

impl StableOrd for u8 {
    const CAN_USE_UNSTABLE_SORT: bool = true;

    // Encoding and decoding doesn't change the bytes of trivial types
    // and `Ord::cmp` depends only on those bytes.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

impl_stable_traits_for_trivial_type!(u16);
impl_stable_traits_for_trivial_type!(u32);
impl_stable_traits_for_trivial_type!(u64);
impl_stable_traits_for_trivial_type!(usize);

impl_stable_traits_for_trivial_type!(u128);
impl_stable_traits_for_trivial_type!(i128);

impl_stable_traits_for_trivial_type!(char);
impl_stable_traits_for_trivial_type!(());

impl_stable_traits_for_trivial_type!(Hash64);

// We need a custom impl as the default hash function will only hash half the bits. For stable
// hashing we want to hash the full 128-bit hash.
impl StableHash for Hash128 {
    #[inline]
    fn stable_hash<Hcx>(&self, _: &mut Hcx, hasher: &mut StableHasher) {
        self.as_u128().hash(hasher);
    }
}

impl StableOrd for Hash128 {
    const CAN_USE_UNSTABLE_SORT: bool = true;

    // Encoding and decoding doesn't change the bytes of `Hash128`
    // and `Ord::cmp` depends only on those bytes.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

// `crate::Never` is `!` spelled on stable; see its definition in `lib.rs`.
impl StableHash for crate::Never {
    fn stable_hash<Hcx>(&self, _hcx: &mut Hcx, _hasher: &mut StableHasher) {
        unreachable!()
    }
}

impl<T> StableHash for PhantomData<T> {
    fn stable_hash<Hcx>(&self, _hcx: &mut Hcx, _hasher: &mut StableHasher) {}
}

// `ZeroablePrimitive` is unstable, so the `NonZero` impls are spelled out per integer type.
macro_rules! impl_stable_hash_for_nonzero {
    ($($ty:ty),*) => {
        $(
            impl StableHash for NonZero<$ty> {
                #[inline]
                fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
                    self.get().stable_hash(hcx, hasher)
                }
            }
        )*
    }
}

impl_stable_hash_for_nonzero!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize);

impl StableHash for f32 {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        let val: u32 = self.to_bits();
        val.stable_hash(hcx, hasher);
    }
}

impl StableHash for f64 {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        let val: u64 = self.to_bits();
        val.stable_hash(hcx, hasher);
    }
}

impl StableHash for ::core::cmp::Ordering {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        (*self as i8).stable_hash(hcx, hasher);
    }
}

impl<T1: StableHash> StableHash for (T1,) {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        let (ref _0,) = *self;
        _0.stable_hash(hcx, hasher);
    }
}

impl<T1: StableHash, T2: StableHash> StableHash for (T1, T2) {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        let (ref _0, ref _1) = *self;
        _0.stable_hash(hcx, hasher);
        _1.stable_hash(hcx, hasher);
    }
}

impl<T1: StableOrd, T2: StableOrd> StableOrd for (T1, T2) {
    const CAN_USE_UNSTABLE_SORT: bool = T1::CAN_USE_UNSTABLE_SORT && T2::CAN_USE_UNSTABLE_SORT;

    // Ordering of tuples is a pure function of their elements' ordering, and since
    // the ordering of each element is stable so must be the ordering of the tuple.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

impl<T1, T2, T3> StableHash for (T1, T2, T3)
where
    T1: StableHash,
    T2: StableHash,
    T3: StableHash,
{
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        let (ref _0, ref _1, ref _2) = *self;
        _0.stable_hash(hcx, hasher);
        _1.stable_hash(hcx, hasher);
        _2.stable_hash(hcx, hasher);
    }
}

impl<T1: StableOrd, T2: StableOrd, T3: StableOrd> StableOrd for (T1, T2, T3) {
    const CAN_USE_UNSTABLE_SORT: bool =
        T1::CAN_USE_UNSTABLE_SORT && T2::CAN_USE_UNSTABLE_SORT && T3::CAN_USE_UNSTABLE_SORT;

    // Ordering of tuples is a pure function of their elements' ordering, and since
    // the ordering of each element is stable so must be the ordering of the tuple.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

impl<T1, T2, T3, T4> StableHash for (T1, T2, T3, T4)
where
    T1: StableHash,
    T2: StableHash,
    T3: StableHash,
    T4: StableHash,
{
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        let (ref _0, ref _1, ref _2, ref _3) = *self;
        _0.stable_hash(hcx, hasher);
        _1.stable_hash(hcx, hasher);
        _2.stable_hash(hcx, hasher);
        _3.stable_hash(hcx, hasher);
    }
}

impl<T1: StableOrd, T2: StableOrd, T3: StableOrd, T4: StableOrd> StableOrd for (T1, T2, T3, T4) {
    const CAN_USE_UNSTABLE_SORT: bool = T1::CAN_USE_UNSTABLE_SORT
        && T2::CAN_USE_UNSTABLE_SORT
        && T3::CAN_USE_UNSTABLE_SORT
        && T4::CAN_USE_UNSTABLE_SORT;

    // Ordering of tuples is a pure function of their elements' ordering, and since
    // the ordering of each element is stable so must be the ordering of the tuple.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

impl<T: StableHash> StableHash for [T] {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.len().stable_hash(hcx, hasher);
        // `u8` overrides this to write the whole slice at once; see `stable_hash_slice`.
        T::stable_hash_slice(self, hcx, hasher);
    }
}

impl<T: StableHash> StableHash for Vec<T> {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self[..].stable_hash(hcx, hasher);
    }
}

impl<K, V, R> StableHash for indexmap::IndexMap<K, V, R>
where
    K: StableHash + Eq + Hash,
    V: StableHash,
    R: BuildHasher,
{
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.len().stable_hash(hcx, hasher);
        for kv in self {
            kv.stable_hash(hcx, hasher);
        }
    }
}

impl<K, R> StableHash for indexmap::IndexSet<K, R>
where
    K: StableHash + Eq + Hash,
    R: BuildHasher,
{
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.len().stable_hash(hcx, hasher);
        for key in self {
            key.stable_hash(hcx, hasher);
        }
    }
}

impl<A, const N: usize> StableHash for SmallVec<[A; N]>
where
    A: StableHash,
{
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self[..].stable_hash(hcx, hasher);
    }
}

impl<T: StableHash> StableHash for ThinVec<T> {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self[..].stable_hash(hcx, hasher);
    }
}

impl<T: ?Sized + StableHash> StableHash for Box<T> {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        (**self).stable_hash(hcx, hasher);
    }
}

impl<T: ?Sized + StableHash> StableHash for ::alloc::rc::Rc<T> {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        (**self).stable_hash(hcx, hasher);
    }
}

impl<T: ?Sized + StableHash> StableHash for ::alloc::sync::Arc<T> {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        (**self).stable_hash(hcx, hasher);
    }
}

impl StableHash for str {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.as_bytes().stable_hash(hcx, hasher);
    }
}

impl StableOrd for &str {
    const CAN_USE_UNSTABLE_SORT: bool = true;

    // Encoding and decoding doesn't change the bytes of string slices
    // and `Ord::cmp` depends only on those bytes.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

impl StableHash for String {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self[..].stable_hash(hcx, hasher);
    }
}

impl StableOrd for String {
    const CAN_USE_UNSTABLE_SORT: bool = true;

    // String comparison only depends on their contents and the
    // contents are not changed by (de-)serialization.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

impl StableHash for bool {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        (if *self { 1u8 } else { 0u8 }).stable_hash(hcx, hasher);
    }
}

impl StableOrd for bool {
    const CAN_USE_UNSTABLE_SORT: bool = true;

    // sort order of bools is not changed by (de-)serialization.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

impl<T> StableHash for Option<T>
where
    T: StableHash,
{
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        if let Some(ref value) = *self {
            1u8.stable_hash(hcx, hasher);
            value.stable_hash(hcx, hasher);
        } else {
            0u8.stable_hash(hcx, hasher);
        }
    }
}

impl<T: StableOrd> StableOrd for Option<T> {
    const CAN_USE_UNSTABLE_SORT: bool = T::CAN_USE_UNSTABLE_SORT;

    // the Option wrapper does not add instability to comparison.
    const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
}

impl<T1, T2> StableHash for Result<T1, T2>
where
    T1: StableHash,
    T2: StableHash,
{
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        mem::discriminant(self).stable_hash(hcx, hasher);
        match *self {
            Ok(ref x) => x.stable_hash(hcx, hasher),
            Err(ref x) => x.stable_hash(hcx, hasher),
        }
    }
}

impl<'a, T> StableHash for &'a T
where
    T: StableHash + ?Sized,
{
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        (**self).stable_hash(hcx, hasher);
    }
}

impl<T> StableHash for ::core::mem::Discriminant<T> {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, _: &mut Hcx, hasher: &mut StableHasher) {
        ::core::hash::Hash::hash(self, hasher);
    }
}

impl<T> StableHash for ::core::range::RangeInclusive<T>
where
    T: StableHash,
{
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.start.stable_hash(hcx, hasher);
        self.last.stable_hash(hcx, hasher);
    }
}

impl<I: Idx, T> StableHash for IndexSlice<I, T>
where
    T: StableHash,
{
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.len().stable_hash(hcx, hasher);
        for v in &self.raw {
            v.stable_hash(hcx, hasher);
        }
    }
}

impl<I: Idx, T> StableHash for IndexVec<I, T>
where
    T: StableHash,
{
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.len().stable_hash(hcx, hasher);
        for v in &self.raw {
            v.stable_hash(hcx, hasher);
        }
    }
}

impl<I: Idx> StableHash for DenseBitSet<I> {
    fn stable_hash<Hcx: StableHashCtxt>(&self, _hcx: &mut Hcx, hasher: &mut StableHasher) {
        ::core::hash::Hash::hash(self, hasher);
    }
}

impl<R: Idx, C: Idx> StableHash for bit_set::BitMatrix<R, C> {
    fn stable_hash<Hcx: StableHashCtxt>(&self, _hcx: &mut Hcx, hasher: &mut StableHasher) {
        ::core::hash::Hash::hash(self, hasher);
    }
}

// `::std::ffi::OsStr` was here. Without `std` an OS string is just `[u8]`, which already has a
// `StableHash` impl above, so this line is gone rather than rewritten - a second impl for `[u8]`
// would be a conflicting implementation.

impl_stable_traits_for_trivial_type!(::eko::path::Path);
impl_stable_traits_for_trivial_type!(::eko::path::PathBuf);

// It is not safe to implement StableHash for HashSet, HashMap or any other collection type
// with unstable but observable iteration order.
// See https://github.com/rust-lang/compiler-team/issues/533 for further information.
// Upstream stated this as `impl !StableHash` for hashbrown's `HashSet` and `HashMap` (every
// hasher, `FxBuildHasher` included). Negative impls are unstable, so the rule is now kept by
// not writing an impl; nothing in this crate needs the negative impl for coherence.

impl<K, V> StableHash for ::alloc::collections::BTreeMap<K, V>
where
    K: StableHash + StableOrd,
    V: StableHash,
{
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.len().stable_hash(hcx, hasher);
        for entry in self.iter() {
            entry.stable_hash(hcx, hasher);
        }
    }
}

impl<K> StableHash for ::alloc::collections::BTreeSet<K>
where
    K: StableHash + StableOrd,
{
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.len().stable_hash(hcx, hasher);
        for entry in self.iter() {
            entry.stable_hash(hcx, hasher);
        }
    }
}

/// Controls what data we do or do not hash.
/// Whenever a `StableHash` implementation caches its
/// result, it needs to include `StableHashControls` as part
/// of the key, to ensure that it does not produce an incorrect
/// result (for example, using a `Fingerprint` produced while
/// hashing `Span`s when a `Fingerprint` without `Span`s is
/// being requested)
#[derive(Clone, Copy, Hash, Eq, PartialEq, Debug)]
pub struct StableHashControls {
    pub hash_spans: bool,
}

// `ADDRESS_CACHE_GENERATION`, `address_cache_generation` and `bump_address_cache_generation`
// were here: a process-wide counter that retired the thread-local, address-keyed stable-hash
// memos of `RawList` and `AdtDefData` whenever a new `GlobalCtxt` could start reusing a dead
// arena's addresses. The memos moved onto the hashing context, where they die with the
// computation that filled them, so there is nothing left to retire. See
// `StableHashCtxt::memoized_address_hash`.
