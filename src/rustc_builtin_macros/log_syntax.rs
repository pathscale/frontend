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
use crate::rustc_ast_pretty::pprust;
use crate::rustc_expand::base::{DummyResult, ExpandResult, ExtCtxt, MacroExpanderResult};

pub(crate) fn expand_log_syntax<'cx>(
    _cx: &'cx mut ExtCtxt<'_>,
    sp: crate::rustc_span::Span,
    tts: TokenStream,
) -> MacroExpanderResult<'cx> {
    eko::println!("{}", pprust::tts_to_string(&tts));

    // any so that `log_syntax` can be invoked as an expression and item.
    ExpandResult::Ready(DummyResult::any_valid(sp))
}
