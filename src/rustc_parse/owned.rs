//! Independent file parsing with owned input, AST, diagnostics and parser effects.
//!
//! Results retain their own identity tables. Inspect symbols and compressed spans inside
//! ParsedFile::enter; ASTs from different files belong to different parsing sessions. This API
//! does not expand modules or combine their identity tables into a compilation.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use parking_lot::Mutex;

use super::lexer::StripTokens;
use super::new_parser_from_source_file;
use crate::rustc_ast as ast;
use crate::rustc_ast::ptr::ParseArena;
use crate::rustc_data_structures::fx::{FxHashMap, FxIndexMap};
use crate::rustc_data_structures::sync;
use crate::rustc_errors::emitter::Emitter;
use crate::rustc_errors::{BufferedEarlyLint, DiagCtxt, DiagCtxtFlags, DiagInner};
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::owned_parse::OwnedParseGlobals;
use crate::rustc_span::source_map::SourceMap;
use crate::rustc_span::{SourceFile, Span, Symbol, parse_transaction};

/// A loaded file and its mapping. Task transfer clones Arc handles, never source bytes.
pub struct ParseFileInput {
    source_file: Arc<SourceFile>,
    source_map: Arc<SourceMap>,
    edition: Edition,
    strip_tokens: StripTokens,
    diagnostic_flags: DiagCtxtFlags,
}

impl ParseFileInput {
    /// Bind a loaded source to the map used for its snippets and diagnostic locations.
    pub fn new(
        source_file: Arc<SourceFile>,
        source_map: Arc<SourceMap>,
        edition: Edition,
        strip_tokens: StripTokens,
    ) -> Self {
        assert!(source_file.src.is_some(), "owned_parse_source_missing");
        assert!(
            source_map.files().iter().any(|file| Arc::ptr_eq(file, &source_file)),
            "owned_parse_source_map_mismatch",
        );
        Self {
            source_file,
            source_map,
            edition,
            strip_tokens,
            diagnostic_flags: DiagCtxtFlags { can_emit_warnings: true, ..Default::default() },
        }
    }

    pub fn with_diagnostic_flags(mut self, flags: DiagCtxtFlags) -> Self {
        self.diagnostic_flags = flags;
        self
    }

    fn strip_tokens(&self) -> StripTokens {
        match self.strip_tokens {
            StripTokens::ShebangAndFrontmatter => StripTokens::ShebangAndFrontmatter,
            StripTokens::Shebang => StripTokens::Shebang,
            StripTokens::Nothing => StripTokens::Nothing,
        }
    }
}

/// Parser side effects in their existing order, without an emitter or session reference.
pub struct FileParseEffects {
    pub lints: Vec<BufferedEarlyLint>,
    pub ambiguous: FxIndexMap<Span, Span>,
    pub gates: FxHashMap<Symbol, Vec<Span>>,
    pub raw_identifier_spans: Vec<Span>,
    pub bad_unicode_identifiers: FxIndexMap<Symbol, Vec<Span>>,
    pub symbols: FxIndexMap<Symbol, Span>,
}

/// One file's owned output. Recovery may produce an AST together with error diagnostics.
pub struct ParsedFile {
    globals: OwnedParseGlobals,
    pub source_file: Arc<SourceFile>,
    pub source_map: Arc<SourceMap>,
    pub krate: Option<ast::Crate>,
    pub diagnostics: Vec<DiagInner>,
    pub effects: FileParseEffects,
    pub has_errors: bool,
    pub fatal: bool,
}

impl ParsedFile {
    /// Inspect or render the output under the identities retained by this file.
    pub fn enter<R>(&self, inspect: impl FnOnce(&Self) -> R) -> R {
        self.globals.enter(|| inspect(self))
    }
}

/// Parse independent files using plain nagoya tasks at the selected session's width.
///
/// One output per input, in input order. Each file owns its diagnostic context and globals;
/// diagnostic order within a file is the parser's original emission order. No borrowed parse
/// session, diagnostic sink or compiler context is handed to the executor. Nested body parsing
/// uses the same application pool budget and its existing transactional serial fallback.
/// A call outside a session uses the configured pool width. An empty batch starts no pool.
pub fn parse_source_files(files: Arc<[ParseFileInput]>) -> Vec<ParsedFile> {
    let len = files.len();
    if len == 0 {
        return Vec::new();
    }
    let _mode = sync::enter_session_width(sync::parse_pool_width());
    sync::run_owned_stage(files, len, file_weight, parse_file)
}

fn file_weight(files: &[ParseFileInput], index: usize) -> u32 {
    files[index].source_file.normalized_source_len.0.max(1)
}

struct Diagnostics {
    records: Arc<Mutex<Vec<DiagInner>>>,
    source_map: Arc<SourceMap>,
}

impl Emitter for Diagnostics {
    fn emit_diagnostic(&mut self, diagnostic: DiagInner) {
        self.records.lock().push(diagnostic);
    }

    fn source_map(&self) -> Option<&SourceMap> {
        Some(&self.source_map)
    }
}

fn parse_file(files: &[ParseFileInput], index: usize) -> ParsedFile {
    let input = &files[index];
    let globals = OwnedParseGlobals::for_file(
        input.edition,
        Arc::clone(&input.source_map),
        input.source_file.normalized_source_len.0 as usize,
    );
    let (krate, diagnostics, effects, has_errors, fatal) = globals.enter(|| {
        // File-local diagnostics never enter an unrelated compiler's query tracking hook.
        // Unlike a body transaction, a complete file owns its symbol and span insertions.
        parse_transaction::file_pass(|| {
            let _arena = ParseArena::enter();
            let records = Arc::new(Mutex::new(Vec::new()));
            let dcx = DiagCtxt::new(Box::new(Diagnostics {
                records: Arc::clone(&records),
                source_map: Arc::clone(&input.source_map),
            }))
            .with_flags(input.diagnostic_flags);
            let mut psess = ParseSess::with_dcx(dcx, Arc::clone(&input.source_map));
            psess.edition = input.edition;
            let parsed = sync::speculate_parse(|| {
                let mut parser = match new_parser_from_source_file(
                    &psess,
                    Arc::clone(&input.source_file),
                    input.strip_tokens(),
                ) {
                    Ok(parser) => parser,
                    Err(diagnostics) => {
                        for diagnostic in diagnostics {
                            diagnostic.emit();
                        }
                        return None;
                    }
                };
                match parser.parse_crate_mod() {
                    Ok(krate) => Some(krate),
                    Err(diagnostic) => {
                        diagnostic.emit();
                        None
                    }
                }
            });
            let fatal = parsed.is_none();
            let krate = parsed.flatten();
            let _ = psess.dcx().emit_stashed_diagnostics();
            psess.dcx().flush_delayed();
            let has_errors = psess.dcx().has_errors().is_some();
            let effects = psess.take_parse_effects();
            let effects = FileParseEffects {
                lints: effects.lints,
                ambiguous: effects.ambiguous,
                gates: effects.gates,
                raw_identifier_spans: psess.raw_identifier_spans.iter().collect(),
                bad_unicode_identifiers: core::mem::take(&mut *psess.bad_unicode_identifiers.lock()),
                symbols: core::mem::take(&mut *psess.symbol_gallery.symbols.lock()),
            };
            // The emitter and all drop-time diagnostics finish before the records move out.
            drop(psess);
            let diagnostics = core::mem::take(&mut *records.lock());
            (krate, diagnostics, effects, has_errors, fatal)
        })
    });
    ParsedFile {
        globals,
        source_file: Arc::clone(&input.source_file),
        source_map: Arc::clone(&input.source_map),
        krate,
        diagnostics,
        effects,
        has_errors,
        fatal,
    }
}
