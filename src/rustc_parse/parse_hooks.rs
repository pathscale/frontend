//! Narrow parser entry point for `examples/parse_oracle.rs`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::ast::{Attribute, Crate};
use crate::rustc_ast::attr::AttrIdGenerator;
use crate::rustc_ast::mut_visit::{self, MutVisitor};
use crate::rustc_data_structures::sync::Lock;
use crate::rustc_errors::emitter::Emitter;
use crate::rustc_errors::{DiagCtxt, DiagCtxtFlags, DiagInner};
use crate::rustc_parse::lexer::StripTokens;
use crate::rustc_parse::parser::oracle::{self, Mode, ModeGuard};
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::source_map::{FilePathMapping, SourceMap};
use crate::rustc_span::{FileName, Span, create_session_globals_then};

use super::new_parser_from_source_str;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseRecord {
    pub status: &'static str,
    pub ast_debug: String,
    pub spans: Vec<(u32, u32, u32)>,
    pub attr_ids: Vec<u32>,
    pub final_attr_id: u32,
    pub continuation: String,
    /// Ordered structured diagnostics, excluding the implementation's emission call site.
    pub diagnostics: Vec<String>,
    pub effects: Vec<String>,
    pub symbol_gallery: Vec<(String, u32, u32)>,
    pub raw_identifier_spans: Vec<(u32, u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compared {
    pub reference: ParseRecord,
    pub candidate: ParseRecord,
    pub width_four: Option<ParseRecord>,
    pub detail: Option<String>,
}

#[derive(Default)]
struct AstCapture {
    spans: Vec<(u32, u32, u32)>,
    attr_ids: Vec<u32>,
}

impl AstCapture {
    fn record_span(&mut self, span: Span) {
        let data = span.data();
        self.spans.push((data.lo.0, data.hi.0, data.ctxt.as_u32()));
    }
}

impl MutVisitor for AstCapture {
    fn visit_span(&mut self, span: &mut Span) {
        self.record_span(*span);
    }

    fn visit_attribute(&mut self, attr: &mut Attribute) {
        self.attr_ids.push(attr.id.as_u32());
        mut_visit::walk_attribute(self, attr);
    }
}

fn capture_ast(ast: &mut Crate) -> (Vec<(u32, u32, u32)>, Vec<u32>) {
    let mut capture = AstCapture::default();
    mut_visit::walk_crate(&mut capture, ast);
    (capture.spans, capture.attr_ids)
}

fn gallery(psess: &ParseSess) -> Vec<(String, u32, u32)> {
    psess
        .symbol_gallery
        .symbols
        .lock()
        .iter()
        .map(|(&symbol, &span)| {
            let data = span.data();
            (String::from(symbol.as_str()), data.lo.0, data.hi.0)
        })
        .collect()
}

fn span_tuples(spans: Vec<Span>) -> Vec<(u32, u32, u32)> {
    spans
        .into_iter()
        .map(|span| {
            let data = span.data();
            (data.lo.0, data.hi.0, data.ctxt.as_u32())
        })
        .collect()
}

struct Diagnostics(Arc<Lock<Vec<String>>>);

fn diagnostic_record(diag: &DiagInner) -> String {
    let mut args: Vec<_> =
        diag.args.iter().map(|(key, value)| format!("{key:?}={value:?}")).collect();
    args.sort();
    format!(
        "{:?} {:?} {:?} {:?} {:?} {:?} {:?}",
        diag.level(),
        diag.messages,
        diag.code,
        diag.span,
        diag.children,
        diag.suggestions,
        args
    )
}

impl Emitter for Diagnostics {
    fn emit_diagnostic(&mut self, diag: DiagInner) {
        self.0.lock().push(diagnostic_record(&diag));
    }

    fn source_map(&self) -> Option<&SourceMap> {
        None
    }
}

fn diagnostics(psess: &ParseSess, records: &Lock<Vec<String>>) -> Vec<String> {
    let _ = psess.dcx().emit_stashed_diagnostics();
    records.lock().clone()
}

// Deferred decorators remain opaque; compare ordered lint identities and source spans.
fn effects(psess: &ParseSess) -> Vec<String> {
    let mut result: Vec<_> = psess
        .buffered_lints
        .lock()
        .iter()
        .map(|lint| format!("lint {} {:?} {:?}", lint.lint_id.lint.name, lint.node_id, lint.span))
        .collect();
    result.extend(
        psess
            .ambiguous_block_expr_parse
            .lock()
            .iter()
            .map(|(left, right)| format!("ambiguous {left:?} {right:?}")),
    );
    let mut gates: Vec<_> = psess
        .gated_spans
        .spans
        .lock()
        .iter()
        .map(|(feature, spans)| (String::from(feature.as_str()), format!("{spans:?}")))
        .collect();
    gates.sort_by(|left, right| left.0.cmp(&right.0));
    result.extend(gates.into_iter().map(|(feature, spans)| format!("gate {feature} {spans}")));
    result
}

fn run_one(source: Arc<String>, edition: Edition, mode: Mode, width: usize) -> ParseRecord {
    let _width = crate::rustc_data_structures::sync::enter_session_width(width);
    create_session_globals_then(edition, &[], None, || {
        let _mode = ModeGuard::set(mode);
        let records = Arc::new(Lock::new(Vec::new()));
        let sm = Arc::new(SourceMap::new(FilePathMapping::empty()));
        let dcx =
            DiagCtxt::new(Box::new(Diagnostics(Arc::clone(&records)))).with_flags(DiagCtxtFlags {
                can_emit_warnings: true,
                eagerly_emit_delayed_bugs: true,
                ..DiagCtxtFlags::default()
            });
        let mut psess = ParseSess::with_dcx(dcx, sm);
        psess.edition = edition;
        let parsed = crate::rustc_errors::catch_fatal_errors(|| {
            let parser = new_parser_from_source_str(
                &psess,
                FileName::Custom(String::from("parse_oracle.rs")),
                Arc::clone(&source),
                StripTokens::Nothing,
            );
            let mut parser = match parser {
                Ok(parser) => parser,
                Err(errors) => {
                    errors.into_iter().for_each(|error| {
                        records.lock().push(diagnostic_record(&error));
                        error.cancel();
                    });
                    return ParseRecord {
                        status: "lex-error",
                        ast_debug: String::new(),
                        spans: Vec::new(),
                        attr_ids: Vec::new(),
                        final_attr_id: attr_position(&psess.attr_id_generator),
                        continuation: String::new(),
                        diagnostics: diagnostics(&psess, &records),
                        effects: effects(&psess),
                        symbol_gallery: gallery(&psess),
                        raw_identifier_spans: span_tuples(
                            psess.raw_identifier_spans.iter().collect(),
                        ),
                    };
                }
            };
            match parser.parse_crate_mod() {
                Ok(mut ast) => {
                    let ast_debug = format!("{ast:?}");
                    let (spans, attr_ids) = capture_ast(&mut ast);
                    let final_attr_id = attr_position(&psess.attr_id_generator);
                    let symbol_gallery = gallery(&psess);
                    let raw_identifier_spans =
                        span_tuples(psess.raw_identifier_spans.iter().collect());
                    ParseRecord {
                        status: "ok",
                        ast_debug,
                        spans,
                        attr_ids,
                        final_attr_id,
                        continuation: crate::rustc_parse::parser::oracle::continuation(&parser),
                        diagnostics: diagnostics(&psess, &records),
                        effects: effects(&psess),
                        symbol_gallery,
                        raw_identifier_spans,
                    }
                }
                Err(error) => {
                    records.lock().push(diagnostic_record(&error));
                    error.cancel();
                    ParseRecord {
                        status: "parse-error",
                        ast_debug: String::new(),
                        spans: Vec::new(),
                        attr_ids: Vec::new(),
                        final_attr_id: attr_position(&psess.attr_id_generator),
                        continuation: crate::rustc_parse::parser::oracle::continuation(&parser),
                        diagnostics: diagnostics(&psess, &records),
                        effects: effects(&psess),
                        symbol_gallery: gallery(&psess),
                        raw_identifier_spans: span_tuples(
                            psess.raw_identifier_spans.iter().collect(),
                        ),
                    }
                }
            }
        });
        parsed.unwrap_or_else(|_| ParseRecord {
            status: "fatal-error",
            ast_debug: String::new(),
            spans: Vec::new(),
            attr_ids: Vec::new(),
            final_attr_id: attr_position(&psess.attr_id_generator),
            continuation: String::new(),
            diagnostics: diagnostics(&psess, &records),
            effects: effects(&psess),
            symbol_gallery: gallery(&psess),
            raw_identifier_spans: span_tuples(psess.raw_identifier_spans.iter().collect()),
        })
    })
}

#[inline]
fn attr_position(generator: &AttrIdGenerator) -> u32 {
    generator.position()
}

pub fn compare(source: Arc<String>, edition: Edition) -> Compared {
    let reference = run_one(Arc::clone(&source), edition, Mode::Off, 1);
    let candidate = run_one(Arc::clone(&source), edition, Mode::Fast, 1);
    let width_four = if cfg!(feature = "parallel") {
        Some(run_one(source, edition, Mode::Fast, 4))
    } else {
        None
    };
    let detail = first_difference(&reference, &candidate)
        .map(|detail| format!("Off/Fast: {detail}"))
        .or_else(|| {
            width_four
                .as_ref()
                .and_then(|record| first_difference(&candidate, record))
                .map(|detail| format!("width 1/4: {detail}"))
        });
    Compared { reference, candidate, width_four, detail }
}

pub fn parse_in_mode(source: Arc<String>, edition: Edition, mode: Mode) -> ParseRecord {
    run_one(source, edition, mode, 1)
}

pub fn reset_oracle_counts() {
    oracle::reset_counts();
}

pub fn oracle_counts() -> oracle::Counts {
    oracle::snapshot_counts()
}

fn first_difference(left: &ParseRecord, right: &ParseRecord) -> Option<String> {
    macro_rules! check {
        ($($field:ident),+ $(,)?) => { $(
            if left.$field != right.$field {
                let left = format!("{:?}", left.$field);
                let right = format!("{:?}", right.$field);
                let offset = left.bytes().zip(right.bytes()).position(|(a,b)| a != b)
                    .unwrap_or(left.len().min(right.len()));
                let excerpt = |text: &str| -> String {
                    text.chars().skip(text[..text.floor_char_boundary(offset)].chars().count().saturating_sub(120))
                        .take(400).collect()
                };
                return Some(format!("{} offset={offset}: reference={} candidate={}",
                    stringify!($field), excerpt(&left), excerpt(&right)));
            }
        )+ };
    }
    check!(
        status,
        ast_debug,
        spans,
        attr_ids,
        final_attr_id,
        diagnostics,
        effects,
        continuation,
        symbol_gallery,
        raw_identifier_spans
    );
    None
}
