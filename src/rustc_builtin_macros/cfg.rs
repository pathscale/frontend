//! The compiler code necessary to support the cfg! extension, which expands to
//! a literal `true` or `false` based on whether the given cfg matches the
//! current compilation environment.

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
use crate::rustc_ast::{AttrStyle, token};
use crate::rustc_attr_ir::target::Target;
use crate::rustc_attr_ir::{AttrPath, CfgEntry};
use crate::rustc_attr_parsing::parser::{AllowExprMetavar, MetaItemOrLitParser};
use crate::rustc_attr_parsing::{
    self as attr, AttributeParser, AttributeSafety, CFG_TEMPLATE, ParsedDescription, ShouldEmit,
    parse_cfg_entry,
};
use crate::rustc_expand::base::{DummyResult, ExpandResult, ExtCtxt, MacEager, MacroExpanderResult};
use crate::exp;
use crate::rustc_parse::parser::Recovery;
use crate::rustc_span::{ErrorGuaranteed, Span, sym};

use crate::rustc_builtin_macros::diagnostics;

pub(crate) fn expand_cfg(
    cx: &mut ExtCtxt<'_>,
    sp: Span,
    tts: TokenStream,
) -> MacroExpanderResult<'static> {
    let sp = cx.with_def_site_ctxt(sp);

    ExpandResult::Ready(match parse_cfg(cx, sp, tts) {
        Ok(cfg) => {
            let matches_cfg = attr::eval_config_entry(cx.sess, &cfg).as_bool();

            MacEager::expr(cx.expr_bool(sp, matches_cfg))
        }
        Err(guar) => DummyResult::any(sp, guar),
    })
}

fn parse_cfg(cx: &ExtCtxt<'_>, span: Span, tts: TokenStream) -> Result<CfgEntry, ErrorGuaranteed> {
    let mut parser = cx.new_parser_from_tts(tts);
    if parser.token == token::Eof {
        return Err(cx.dcx().emit_err(diagnostics::RequiresCfgPattern { span }));
    }

    let meta = MetaItemOrLitParser::parse_single(
        &mut parser,
        ShouldEmit::ErrorsAndLints { recovery: Recovery::Allowed },
        AllowExprMetavar::Yes,
    )
    .map_err(|diag| diag.emit())?;
    let cfg = AttributeParser::parse_single_args(
        cx.sess,
        span,
        span,
        AttrStyle::Inner,
        AttrPath { segments: vec![sym::cfg].into_boxed_slice(), span },
        None,
        AttributeSafety::Normal,
        ParsedDescription::Macro,
        span,
        cx.current_expansion.lint_node_id,
        // Doesn't matter what the target actually is here.
        Target::Crate,
        Some(cx.ecfg.features),
        ShouldEmit::ErrorsAndLints { recovery: Recovery::Allowed },
        &meta,
        parse_cfg_entry,
        &CFG_TEMPLATE,
    )?;

    let _ = parser.eat(exp!(Comma));

    if !parser.eat(exp!(Eof)) {
        return Err(cx.dcx().emit_err(diagnostics::OneCfgPattern { span }));
    }

    Ok(cfg)
}
