//! `P<T>`, the owning pointer for boxed AST nodes.
//!
//! A one-word owning handle with the parts of `Box` the AST uses: `Deref`, `DerefMut`, a deep
//! `Clone`, and `into_inner` in place of moving out with `*p`. Keeping it a distinct type lets
//! the storage behind it change (an arena) without touching the code that uses it.

use alloc::boxed::Box;
use core::fmt;
use core::ops::{Deref, DerefMut};

use crate::rustc_data_structures::stable_hash::{StableHash, StableHashCtxt, StableHasher};
use crate::rustc_serialize::{Decodable, Decoder, Encodable, Encoder};

#[repr(transparent)]
pub struct P<T> {
    inner: Box<T>,
}

impl<T> P<T> {
    #[inline]
    pub fn new(value: T) -> P<T> {
        P { inner: Box::new(value) }
    }

    /// Moves the node out, which `*p` does for a `Box`.
    #[inline]
    pub fn into_inner(self) -> T {
        *self.inner
    }

    #[inline]
    pub fn map(self, f: impl FnOnce(T) -> T) -> P<T> {
        P::new(f(self.into_inner()))
    }
}

impl<T> Deref for P<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for P<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: Clone> Clone for P<T> {
    #[inline]
    fn clone(&self) -> P<T> {
        P::new((**self).clone())
    }
}

impl<T: fmt::Debug> fmt::Debug for P<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: fmt::Display> fmt::Display for P<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> From<T> for P<T> {
    #[inline]
    fn from(value: T) -> P<T> {
        P::new(value)
    }
}

impl<T> AsRef<T> for P<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T> AsMut<T> for P<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: PartialEq> PartialEq for P<T> {
    fn eq(&self, other: &P<T>) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for P<T> {}

impl<T: core::hash::Hash> core::hash::Hash for P<T> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl<S: Encoder, T: Encodable<S>> Encodable<S> for P<T> {
    fn encode(&self, s: &mut S) {
        (**self).encode(s)
    }
}

impl<D: Decoder, T: Decodable<D>> Decodable<D> for P<T> {
    fn decode(d: &mut D) -> P<T> {
        P::new(Decodable::decode(d))
    }
}

impl<T: StableHash> StableHash for P<T> {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        (**self).stable_hash(hcx, hasher);
    }
}
