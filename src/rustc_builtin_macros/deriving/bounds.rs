// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_ast::{MetaItem, Safety};
use crate::rustc_expand::base::{Annotatable, ExtCtxt};
use crate::rustc_span::Span;

use crate::rustc_builtin_macros::deriving::generic::*;
use crate::rustc_builtin_macros::deriving::path_std;

pub(crate) fn expand_deriving_copy(
    cx: &ExtCtxt<'_>,
    span: Span,
    mitem: &MetaItem,
    item: &Annotatable,
    push: &mut dyn FnMut(Annotatable),
    is_const: bool,
) {
    let trait_def = TraitDef {
        span,
        path: path_std!(marker::Copy),
        skip_path_as_bound: false,
        needs_copy_as_bound_if_packed: false,
        additional_bounds: SmallVec::new(),
        supports_unions: true,
        methods: SmallVec::new(),
        associated_types: SmallVec::new(),
        is_const,
        safety: Safety::Default,
        document: true,
    };

    trait_def.expand(cx, mitem, item, push);
}

pub(crate) fn expand_deriving_const_param_ty(
    cx: &ExtCtxt<'_>,
    span: Span,
    mitem: &MetaItem,
    item: &Annotatable,
    push: &mut dyn FnMut(Annotatable),
    is_const: bool,
) {
    let trait_def = TraitDef {
        span,
        path: path_std!(marker::ConstParamTy_),
        skip_path_as_bound: false,
        needs_copy_as_bound_if_packed: false,
        additional_bounds: smallvec![ty::Ty::Path(path_std!(cmp::Eq))],
        supports_unions: false,
        methods: SmallVec::new(),
        associated_types: SmallVec::new(),
        is_const,
        safety: Safety::Default,
        document: true,
    };

    trait_def.expand(cx, mitem, item, push);
}
