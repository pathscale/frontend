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
use crate::rustc_ast::{AttrVec, VisibilityKind, ast, token};
use crate::rustc_expand::base::{DummyResult, ExpandResult, ExtCtxt, MacEager, MacroExpanderResult};
use crate::rustc_span::Span;
use smallvec::SmallVec;

use crate::rustc_builtin_macros::diagnostics;

pub(crate) fn expand<'cx>(
    cx: &'cx mut ExtCtxt<'_>,
    span: Span,
    tts: TokenStream,
) -> MacroExpanderResult<'cx> {
    let name = "test_binder_constraints!";
    let mut p = cx.new_parser_from_tts(tts);
    if p.token == token::Eof {
        cx.dcx().emit_err(diagnostics::OnlyOneArgument { span, name });
    };
    let item = match p.parse_test_binder_constraints() {
        Ok(expr) => expr,
        Err(diag) => {
            let guar = diag.emit();
            return ExpandResult::Ready(DummyResult::any(span, guar));
        }
    };
    if p.token != token::Eof {
        cx.dcx().emit_err(diagnostics::OnlyOneArgument { span: p.token.span, name });
    }
    let item = Box::new(ast::Item {
        attrs: AttrVec::default(),
        id: ast::DUMMY_NODE_ID,
        span,
        vis: ast::Visibility { kind: VisibilityKind::Inherited, span: span.shrink_to_lo() },
        kind: ast::ItemKind::TestBinderConstraints(item),
        tokens: None,
    });
    crate::rustc_expand::base::ExpandResult::Ready(Box::new(MacEager {
        expr: None,
        items: Some(SmallVec::from_buf([item])),
        ty: None,
    }))
}
