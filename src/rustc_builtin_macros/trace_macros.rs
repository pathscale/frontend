// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_ast::tokenstream::{TokenStream, TokenTree};
use crate::rustc_expand::base::{DummyResult, ExpandResult, ExtCtxt, MacroExpanderResult};
use crate::rustc_span::{Span, kw};

use crate::rustc_builtin_macros::diagnostics;

pub(crate) fn expand_trace_macros(
    cx: &mut ExtCtxt<'_>,
    sp: Span,
    tt: TokenStream,
) -> MacroExpanderResult<'static> {
    let mut iter = tt.iter();
    let mut err = false;
    let value = match iter.next() {
        Some(TokenTree::Token(token, _)) if token.is_keyword(kw::True) => true,
        Some(TokenTree::Token(token, _)) if token.is_keyword(kw::False) => false,
        _ => {
            err = true;
            false
        }
    };
    err |= iter.next().is_some();
    if err {
        cx.dcx().emit_err(diagnostics::TraceMacros { span: sp });
    } else {
        cx.set_trace_macros(value);
    }

    ExpandResult::Ready(DummyResult::any_valid(sp))
}
