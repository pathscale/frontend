// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_ast::ast;
use crate::rustc_ast::tokenstream::TokenStream;
use crate::rustc_expand::base::{self, DummyResult, ExpandResult, ExtCtxt, MacroExpanderResult};
use crate::rustc_span::Span;

use crate::rustc_builtin_macros::util::get_single_expr_from_tts;

pub(crate) fn expand<'cx>(
    cx: &'cx mut ExtCtxt<'_>,
    span: Span,
    tts: TokenStream,
) -> MacroExpanderResult<'cx> {
    let ExpandResult::Ready(expr) = get_single_expr_from_tts(cx, span, tts, "direct_const_arg!")
    else {
        return ExpandResult::Retry(());
    };
    let expr = match expr {
        Ok(expr) => expr,
        Err(err) => return ExpandResult::Ready(DummyResult::any(span, err)),
    };

    let id = ast::DUMMY_NODE_ID;
    ExpandResult::Ready(Box::new(base::MacEager {
        expr: Some(Box::new(ast::Expr {
            id,
            kind: ast::ExprKind::DirectConstArg(expr.clone()),
            span,
            attrs: Default::default(),
            tokens: None,
        })),
        ty: Some(Box::new(ast::Ty { id, kind: ast::TyKind::DirectConstArg(expr), span })),
        ..Default::default()
    }))
}
