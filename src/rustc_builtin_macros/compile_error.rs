// The compiler code necessary to support the compile_error! extension.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_ast::tokenstream::TokenStream;
use crate::rustc_expand::base::{DummyResult, ExpandResult, ExtCtxt, MacroExpanderResult};
use crate::rustc_span::Span;

use crate::rustc_builtin_macros::util::get_single_str_from_tts;

pub(crate) fn expand_compile_error<'cx>(
    cx: &'cx mut ExtCtxt<'_>,
    sp: Span,
    tts: TokenStream,
) -> MacroExpanderResult<'cx> {
    let ExpandResult::Ready(mac) = get_single_str_from_tts(cx, sp, tts, "compile_error!") else {
        return ExpandResult::Retry(());
    };
    let var = match mac {
        Ok(var) => var,
        Err(guar) => return ExpandResult::Ready(DummyResult::any(sp, guar)),
    };

    let guar = cx.dcx().span_err(sp, var.to_string());
    cx.resolver.mark_scope_with_compile_error(cx.current_expansion.lint_node_id);

    ExpandResult::Ready(DummyResult::any(sp, guar))
}
