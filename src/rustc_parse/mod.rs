//! The main parser interface.

// tidy-alphabetical-start
// tidy-alphabetical-end

// Reads source from disk - `fs::read` for a module file, `read_dir` when suggesting the
// path of one that is missing. That is the file-loading surface: an embedded compiler is handed source
// by its caller, and `Config::file_loader` is already the seam for replacing it.

// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// The crate root's no_std attribute excludes the standard-library extern prelude.
// Keep that boundary intact when resolving an import error. Whatever
// needed `std` either has a `core`/`alloc` equivalent, belongs in `ekostd`, or is a
// dependency that has to be replaced.
//
// The prelude is the part a grep cannot see: `Vec`, `String`, `Box`, `format!`, `vec!`,
// `thread_local!` and `println!` name no path. Under `#![no_std]` they resolve through `alloc`
// and `eko` instead, which is why those imports appear at the top of every file here.
// ---------------------------------------------------------------------------------------------
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::str::Utf8Error;
use core::sync::atomic::{AtomicU64, Ordering};
use eko::path::{Path, PathBuf};

use crate::rustc_ast as ast;
use crate::rustc_ast::ptr::{arena_metrics, reset_arena_metrics};
use crate::rustc_ast::token;
use crate::rustc_ast::tokenstream::{DelimSpacing, DelimSpan, Spacing, TokenStream, TokenTree};
use crate::rustc_ast_pretty::pprust;
use crate::rustc_errors::{Diag, EmissionGuarantee, FatalError, PResult, pluralize};
pub use crate::rustc_lexer::UNICODE_VERSION;
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::source_map::SourceMap;
use crate::rustc_span::{FileName, SourceFile, Span};

pub const MACRO_ARGUMENTS: Option<&str> = Some("macro arguments");

static TAPE_ADMISSIONS: AtomicU64 = AtomicU64::new(0);
static TAPE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static PARALLEL_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static PARALLEL_COMMITS: AtomicU64 = AtomicU64::new(0);
static PARALLEL_RETRIES: AtomicU64 = AtomicU64::new(0);
static PARALLEL_BODY_JOBS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParseFastPathMetrics {
    pub tape_admissions: u64,
    pub tape_fallbacks: u64,
    pub parallel_attempts: u64,
    pub parallel_commits: u64,
    pub parallel_retries: u64,
    pub parallel_body_jobs: u64,
    pub chunk_allocs: u64,
    pub chunk_batch_reserves: u64,
    pub chunk_atomic_releases: u64,
    pub chunk_deallocs: u64,
}

#[inline]
pub fn fast_path_metrics() -> ParseFastPathMetrics {
    let arena = arena_metrics();
    ParseFastPathMetrics {
        tape_admissions: TAPE_ADMISSIONS.load(Ordering::Relaxed),
        tape_fallbacks: TAPE_FALLBACKS.load(Ordering::Relaxed),
        parallel_attempts: PARALLEL_ATTEMPTS.load(Ordering::Relaxed),
        parallel_commits: PARALLEL_COMMITS.load(Ordering::Relaxed),
        parallel_retries: PARALLEL_RETRIES.load(Ordering::Relaxed),
        parallel_body_jobs: PARALLEL_BODY_JOBS.load(Ordering::Relaxed),
        chunk_allocs: arena.chunk_allocs,
        chunk_batch_reserves: arena.chunk_batch_reserves,
        chunk_atomic_releases: arena.chunk_atomic_releases,
        chunk_deallocs: arena.chunk_deallocs,
    }
}

#[inline]
pub fn reset_fast_path_metrics() {
    TAPE_ADMISSIONS.store(0, Ordering::Relaxed);
    TAPE_FALLBACKS.store(0, Ordering::Relaxed);
    PARALLEL_ATTEMPTS.store(0, Ordering::Relaxed);
    PARALLEL_COMMITS.store(0, Ordering::Relaxed);
    PARALLEL_RETRIES.store(0, Ordering::Relaxed);
    PARALLEL_BODY_JOBS.store(0, Ordering::Relaxed);
    reset_arena_metrics();
}

#[inline]
pub(crate) fn record_tape_admission() {
    TAPE_ADMISSIONS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn record_tape_fallback() {
    TAPE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "parallel")]
#[inline]
pub(crate) fn record_parallel_attempt() {
    PARALLEL_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "parallel")]
#[inline]
pub(crate) fn record_parallel_commit() {
    PARALLEL_COMMITS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "parallel")]
#[inline]
pub(crate) fn record_parallel_retry() {
    PARALLEL_RETRIES.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "parallel")]
#[inline]
pub(crate) fn record_parallel_body_jobs(count: usize) {
    PARALLEL_BODY_JOBS.fetch_add(count as u64, Ordering::Relaxed);
}

#[macro_use]
pub mod parser;
use parser::Parser;

use crate::rustc_parse::lexer::StripTokens;

#[doc(hidden)]
pub mod lex_hooks;
pub mod lexer;
#[cfg(feature = "parallel")]
pub mod owned;
#[doc(hidden)]
pub mod parse_hooks;

mod diagnostics;

// Make sure that the Unicode version of the dependencies is the same.
const _: () = {
    let rustc_lexer = crate::rustc_lexer::UNICODE_VERSION;
    let rustc_span = crate::rustc_span::UNICODE_VERSION;
    let normalization = unicode_normalization::UNICODE_VERSION;
    let width = unicode_width::UNICODE_VERSION;

    if rustc_lexer.0 != rustc_span.0
        || rustc_lexer.1 != rustc_span.1
        || rustc_lexer.2 != rustc_span.2
    {
        panic!(
            "rustc_lexer and rustc_span must use the same Unicode version, \
            `crate::rustc_lexer::UNICODE_VERSION` and `crate::rustc_span::UNICODE_VERSION` are \
            different."
        );
    }

    if rustc_lexer.0 != normalization.0
        || rustc_lexer.1 != normalization.1
        || rustc_lexer.2 != normalization.2
    {
        panic!(
            "rustc_lexer and unicode-normalization must use the same Unicode version, \
            `crate::rustc_lexer::UNICODE_VERSION` and `unicode_normalization::UNICODE_VERSION` are \
            different."
        );
    }

    if rustc_lexer.0 != width.0 || rustc_lexer.1 != width.1 || rustc_lexer.2 != width.2 {
        panic!(
            "rustc_lexer and unicode-width must use the same Unicode version, \
            `crate::rustc_lexer::UNICODE_VERSION` and `unicode_width::UNICODE_VERSION` are \
            different."
        );
    }
};

// Unwrap the result if `Ok`, otherwise emit the diagnostics and abort.
pub fn unwrap_or_emit_fatal<T>(expr: Result<T, Vec<Diag<'_>>>) -> T {
    match expr {
        Ok(expr) => expr,
        Err(errs) => {
            for err in errs {
                err.emit();
            }
            FatalError.raise()
        }
    }
}

/// Creates a new parser from a source string.
///
/// On failure, the errors must be consumed via `unwrap_or_emit_fatal`, `emit`, `cancel`,
/// etc., otherwise a panic will occur when they are dropped.
pub fn new_parser_from_source_str(
    psess: &ParseSess,
    name: FileName,
    source: impl Into<Arc<String>>,
    strip_tokens: StripTokens,
) -> Result<Parser<'_>, Vec<Diag<'_>>> {
    let source_file = psess.source_map().new_source_file(name, source);
    new_parser_from_source_file(psess, source_file, strip_tokens)
}

/// Creates a new parser from a filename. On failure, the errors must be consumed via
/// `unwrap_or_emit_fatal`, `emit`, `cancel`, etc., otherwise a panic will occur when they are
/// dropped.
///
/// If a span is given, that is used on an error as the source of the problem.
///
/// Error messages are tailored to the specific error kind.
pub fn new_parser_from_file<'a>(
    psess: &'a ParseSess,
    path: &Path,
    strip_tokens: StripTokens,
    sp: Option<Span>,
) -> Result<Parser<'a>, Vec<Diag<'a>>> {
    let sm = psess.source_map();
    let source_file = sm.load_file(path).unwrap_or_else(|e| {
        use eko::file::ErrorKind;

        let msg = match e.kind() {
            ErrorKind::NotFound => format!("couldn't find file `{}`", path.display()),
            ErrorKind::PermissionDenied => {
                format!("permission denied when opening file `{}`", path.display())
            }
            ErrorKind::IsADirectory => format!("`{}` is a directory", path.display()),
            _ => format!("couldn't read `{}`: {}", path.display(), e),
        };

        let mut err = psess.dcx().struct_fatal(msg);
        if let Some(sp) = sp {
            err.span(sp);
        }
        err.emit()
    });
    new_parser_from_source_file(psess, source_file, strip_tokens)
}

pub fn utf8_error<E: EmissionGuarantee>(
    sm: &SourceMap,
    path: &str,
    sp: Option<Span>,
    err: &mut Diag<'_, E>,
    utf8err: Utf8Error,
    contents: &[u8],
) {
    // The file exists, but it wasn't valid UTF-8.
    let start = utf8err.valid_up_to();
    let note = format!("invalid utf-8 at byte `{start}`");
    let msg = if let Some(len) = utf8err.error_len() {
        format!(
            "byte{s} `{bytes}` {are} not valid utf-8",
            bytes = if len == 1 {
                format!("{:?}", contents[start])
            } else {
                format!("{:?}", &contents[start..start + len])
            },
            s = pluralize!(len),
            are = if len == 1 { "is" } else { "are" },
        )
    } else {
        note.clone()
    };
    let contents = String::from_utf8_lossy(contents).into_owned();

    // We only emit this error for files in the current session
    // so the working directory can only be the current working directory
    let filename = FileName::Real(
        sm.path_mapping().to_real_filename(sm.working_dir(), PathBuf::from(path).as_path()),
    );
    let source = sm.new_source_file(filename, contents);

    // Avoid out-of-bounds span from lossy UTF-8 conversion.
    if start as u32 > source.normalized_source_len.0 {
        err.note(note);
        return;
    }

    let span = Span::with_root_ctxt(
        source.normalized_byte_pos(start as u32),
        source.normalized_byte_pos(start as u32),
    );
    if span.is_dummy() {
        err.note(note);
    } else {
        if sp.is_some() {
            err.span_note(span, msg);
        } else {
            err.span(span);
            err.span_label(span, msg);
        }
    }
}

/// Given a session and a `source_file`, return a parser. Returns any buffered errors from lexing
/// the initial token stream.
fn new_parser_from_source_file(
    psess: &ParseSess,
    source_file: Arc<SourceFile>,
    strip_tokens: StripTokens,
) -> Result<Parser<'_>, Vec<Diag<'_>>> {
    let end_pos = source_file.end_position();
    let src = source_file.src.as_ref().unwrap_or_else(|| {
        psess.dcx().bug(format!(
            "cannot lex `source_file` without source: {}",
            psess.source_map().filename_for_diagnostics(&source_file.name)
        ));
    });
    let mut parser = if let Some(cursor) =
        lexer::try_source_cursor(psess, src, source_file.start_pos, &strip_tokens)
    {
        record_tape_admission();
        // The lexer met every newline: hand them to the line table, which otherwise scans the
        // text again the first time a span is looked up (it checks them before taking them).
        if let Some(tape) = cursor.lexed_tape() {
            source_file.supply_precomputed_newlines(tape.newlines());
        }
        Parser::new_with_cursor(psess, cursor, None)
    } else {
        record_tape_fallback();
        let buffer = lexer::lex_flat_tokens(
            psess,
            src.as_str(),
            Some(src),
            source_file.start_pos,
            None,
            strip_tokens,
        )?;
        Parser::from_flat(psess, buffer)
    };
    if parser.token == token::Eof {
        parser.token.span = Span::new(end_pos, end_pos, parser.token.span.ctxt(), None);
    }
    Ok(parser)
}

/// Given a source string, produces a sequence of token trees.
///
/// NOTE: This only strips shebangs, not frontmatter!
pub fn source_str_to_stream(
    psess: &ParseSess,
    name: FileName,
    source: impl Into<Arc<String>>,
    override_span: Option<Span>,
) -> Result<TokenStream, Vec<Diag<'_>>> {
    let source_file = psess.source_map().new_source_file(name, source);
    // FIXME(frontmatter): Consider stripping frontmatter in a future edition. We can't strip them
    // in the current edition since that would be breaking.
    // See also <https://github.com/rust-lang/rust/issues/145520>.
    // Alternatively, stop stripping shebangs here, too, if T-lang and crater approve.
    source_file_to_stream(psess, source_file, override_span, StripTokens::Shebang)
}

/// Given a source file, produces a sequence of token trees.
///
/// Returns any buffered errors from parsing the token stream.
fn source_file_to_stream<'psess>(
    psess: &'psess ParseSess,
    source_file: Arc<SourceFile>,
    override_span: Option<Span>,
    strip_tokens: StripTokens,
) -> Result<TokenStream, Vec<Diag<'psess>>> {
    let src = source_file.src.as_ref().unwrap_or_else(|| {
        psess.dcx().bug(format!(
            "cannot lex `source_file` without source: {}",
            psess.source_map().filename_for_diagnostics(&source_file.name)
        ));
    });

    lexer::lex_token_trees(
        psess,
        src.as_str(),
        Some(src),
        source_file.start_pos,
        override_span,
        strip_tokens,
    )
}

/// Runs the given subparser `f` on the tokens of the given `attr`'s item.
pub fn parse_in<'a, T>(
    psess: &'a ParseSess,
    tts: TokenStream,
    name: &'static str,
    mut f: impl FnMut(&mut Parser<'a>) -> PResult<'a, T>,
) -> PResult<'a, T> {
    let mut parser = Parser::new(psess, tts, Some(name));
    let result = f(&mut parser)?;
    if parser.token != token::Eof {
        parser.unexpected()?;
    }
    Ok(result)
}

pub fn fake_token_stream_for_item(
    psess: &ParseSess,
    item: &ast::Item,
    attr_to_exclude: Option<&ast::Attribute>,
) -> TokenStream {
    if let Some(tokens) = fake_token_stream_for_file_mod(psess, item, attr_to_exclude) {
        return tokens;
    }

    let source = pprust::item_to_string(item);
    let filename = FileName::macro_expansion_source_code(&source);
    unwrap_or_emit_fatal(source_str_to_stream(psess, filename, source, Some(item.span)))
}

fn fake_token_stream_for_file_mod(
    psess: &ParseSess,
    item: &ast::Item,
    attr_to_exclude: Option<&ast::Attribute>,
) -> Option<TokenStream> {
    let ast::ItemKind::Mod(_, _, ast::ModKind::Loaded(_, ast::Inline::No { .. }, spans)) =
        &item.kind
    else {
        return None;
    };

    let attr = attr_to_exclude.expect("file modules must have an attribute to exclude");
    assert_eq!(attr.style, ast::AttrStyle::Inner);

    let mut body_tts = Vec::new();
    body_tts.extend(lex_token_trees_for_span(psess, spans.inner_span.until(attr.span))?);
    body_tts.extend(lex_token_trees_for_span(
        psess,
        attr.span.between(spans.inner_span.shrink_to_hi()),
    )?);

    let mut wrapper_tts = Vec::new();
    for attr in item.attrs.iter().filter(|attr| attr.style == ast::AttrStyle::Outer) {
        wrapper_tts.extend(attr.token_trees());
    }
    wrapper_tts.extend(lex_token_trees_for_span(psess, item.span)?);
    let Some(TokenTree::Token(semi, _)) = wrapper_tts.pop() else {
        return None;
    };
    if semi.kind != token::Semi {
        return None;
    }
    wrapper_tts.push(TokenTree::Delimited(
        DelimSpan::from_single(semi.span),
        DelimSpacing::new(Spacing::Alone, Spacing::Alone),
        token::Delimiter::Brace,
        TokenStream::new(body_tts),
    ));

    Some(TokenStream::new(wrapper_tts))
}

fn lex_token_trees_for_span(
    psess: &ParseSess,
    span: Span,
) -> Option<impl Iterator<Item = TokenTree>> {
    let src = psess.source_map().span_to_snippet(span).ok()?;
    let stream =
        match lexer::lex_token_trees(psess, &src, None, span.lo(), None, StripTokens::Nothing) {
            Ok(stream) => stream,
            Err(errs) => {
                errs.into_iter().for_each(|err| err.cancel());
                return None;
            }
        };
    Some((0..).map_while(move |index| stream.get(index).cloned()))
}

pub fn fake_token_stream_for_foreign_item(
    psess: &ParseSess,
    item: &ast::ForeignItem,
) -> TokenStream {
    let source = pprust::foreign_item_to_string(item);
    let filename = FileName::macro_expansion_source_code(&source);
    unwrap_or_emit_fatal(source_str_to_stream(psess, filename, source, Some(item.span)))
}

pub fn fake_token_stream_for_crate(psess: &ParseSess, krate: &ast::Crate) -> TokenStream {
    let source = pprust::crate_to_string_for_macros(krate);
    let filename = FileName::macro_expansion_source_code(&source);
    unwrap_or_emit_fatal(source_str_to_stream(
        psess,
        filename,
        source,
        Some(krate.spans.inner_span),
    ))
}
pub use crate::exp;
pub use crate::maybe_recover_from_interpolated_ty_qpath;
