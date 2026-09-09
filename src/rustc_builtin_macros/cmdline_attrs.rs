//! Attributes injected into the crate root from command line using `-Z crate-attr`.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_ast as ast;
use crate::rustc_errors::Diag;
use crate::rustc_parse::parser::attr::InnerAttrPolicy;
use crate::rustc_parse::{parse_in, source_str_to_stream};
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::FileName;

pub fn inject(krate: &mut ast::Crate, psess: &ParseSess, attrs: &[String]) {
    for raw_attr in attrs {
        let source = format!("#![{raw_attr}]");
        let parse = || -> Result<ast::Attribute, Vec<Diag<'_>>> {
            let tokens = source_str_to_stream(
                psess,
                FileName::cli_crate_attr_source_code(raw_attr),
                source,
                None,
            )?;
            parse_in(psess, tokens, "<crate attribute>", |p| {
                p.parse_attribute(InnerAttrPolicy::Permitted)
            })
            .map_err(|e| vec![e])
        };
        let meta = match parse() {
            Ok(meta) => meta,
            Err(errs) => {
                for err in errs {
                    err.emit();
                }
                continue;
            }
        };

        krate.attrs.push(meta);
    }
}
