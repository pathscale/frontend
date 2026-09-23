// Vendored from rustc-stable-hash 0.1.2, converted to `no_std`. See
// `vendor/rustc-stable-hash/VENDORED.md`.
//
// The conversion is a rename. Every `std::` path in this crate was already `core`-reachable -
// `std::hash`, `std::mem`, `std::ptr`, `std::fmt` - and nothing here allocates, so there is no
// `extern crate alloc`. The hashing itself is untouched, which is the point: this is the
// algorithm behind `DepNode` identity and a behavioural change would be a silent miscompile.
//! A stable hashing algorithm used by rustc

#![deny(clippy::missing_safety_doc)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unreachable_pub)]

mod int_overflow;
mod sip128;
mod stable_hasher;

/// Hashers collection
pub mod hashers {
    #[doc(inline)]
    pub use super::sip128::{SipHasher128, SipHasher128Hash};

    /// Stable 128-bits Sip Hasher
    ///
    /// [`StableHasher`] version of [`SipHasher128`].
    ///
    /// [`StableHasher`]: super::StableHasher
    pub type StableSipHasher128 = super::StableHasher<SipHasher128>;
}

#[doc(inline)]
pub use stable_hasher::StableHasher;

#[doc(inline)]
pub use stable_hasher::FromStableHash;

#[doc(inline)]
pub use stable_hasher::ExtendedHasher;

#[doc(inline)]
pub use hashers::{SipHasher128Hash, StableSipHasher128};
