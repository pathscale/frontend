use derive_where::derive_where;
use rustc_macros::GenericTypeVisitable;
#[cfg(feature = "nightly")]
use rustc_macros::{Decodable_NoContext, Encodable_NoContext, StableHash_NoContext};

use crate::rustc_type_ir::{Interner, Region};

#[derive_where(Clone, Copy, PartialEq, Debug; I: Interner)]
#[derive(GenericTypeVisitable)]
#[cfg_attr(
    feature = "nightly",
    derive(Decodable_NoContext, Encodable_NoContext, StableHash_NoContext)
)]
pub enum GenericArgKind<I: Interner> {
    Lifetime(Region<I>),
    Type(I::Ty),
    Const(I::Const),
}

impl<I: Interner> Eq for GenericArgKind<I> {}
