// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.

use crate::rustc_span::Symbol;
use crate::rustc_span::def_id::DefId;
use rustc_macros::{Decodable, Encodable, StableHash};

use super::TyCtxt;

#[derive(Copy, Clone, Debug, Decodable, Encodable, StableHash)]
pub struct IntrinsicDef {
    pub name: Symbol,
    /// Whether the intrinsic has no meaningful body and all backends need to shim all calls to it.
    pub must_be_overridden: bool,
    /// Whether the intrinsic can be invoked from stable const fn
    pub const_stable: bool,
}

impl TyCtxt<'_> {
    pub fn is_intrinsic(self, def_id: DefId, name: Symbol) -> bool {
        let Some(i) = self.intrinsic(def_id) else { return false };
        i.name == name
    }
}
