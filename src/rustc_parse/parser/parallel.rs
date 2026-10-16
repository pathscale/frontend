//! Transactional function-body parsing over owned, already lexed token subtrees.
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
mod count;
mod state;
use super::Parser;
use crate::exp;
use crate::rustc_ast as ast;
use crate::rustc_ast::attr::AttrIdGenerator;
use crate::rustc_ast::mut_visit::{self, FnKind, MutVisitor};
use crate::rustc_ast::ptr::P;
use crate::rustc_ast::token::{self, Delimiter, Token};
use crate::rustc_ast::tokenstream::{TokenStream, TokenTree};
use crate::rustc_data_structures::fx::FxHashMap;
use crate::rustc_data_structures::sync::{self, Lock};
use crate::rustc_errors::emitter::Emitter;
use crate::rustc_errors::{DiagCtxt, DiagCtxtFlags, DiagInner, PResult};
use crate::rustc_session::parse::{ParseEffects, ParseSess};
use crate::rustc_span::edition::Edition;
use crate::rustc_span::owned_parse::OwnedParseGlobals;
use crate::rustc_span::source_map::SourceMap;
use crate::rustc_span::{Ident, Span, with_parse_globals};
use crate::rustc_span::{parse_transaction, with_session_globals};
use count::count;
use state::State;
pub(super) struct BodyJob {
    ident: Span,
    lo: Span,
    maybe_ident: Token,
    state: State,
    attr_start: u32,
    attr_end: u32,
    end_bumps: u32,
    before: ParseEffects,
}
struct Input {
    globals: OwnedParseGlobals,
    source_map: Arc<SourceMap>,
    jobs: Vec<BodyJob>,
    edition: Edition,
}
struct Body {
    block: P<ast::Block>,
    effects: ParseEffects,
}
struct Candidate {
    krate: ast::Crate,
    state: State,
    effects: Vec<ParseEffects>,
    attr_end: u32,
}
// The original emitter is never replaced. Any speculative diagnostic refuses commit,
// including warnings, stashes flushed at the boundary and eagerly emitted delayed bugs.
struct DiagnosticFlag(Arc<AtomicBool>);
impl Emitter for DiagnosticFlag {
    fn emit_diagnostic(&mut self, _: DiagInner) {
        self.0.store(true, Ordering::Relaxed);
    }
    fn source_map(&self) -> Option<&SourceMap> {
        None
    }
}
fn session(
    source_map: Arc<SourceMap>,
    attr_start: u32,
    edition: Edition,
) -> (ParseSess, Arc<AtomicBool>) {
    let diagnostic = Arc::new(AtomicBool::new(false));
    let dcx = DiagCtxt::new(Box::new(DiagnosticFlag(Arc::clone(&diagnostic)))).with_flags(
        DiagCtxtFlags {
            can_emit_warnings: true,
            eagerly_emit_delayed_bugs: true,
            ..DiagCtxtFlags::default()
        },
    );
    let mut psess = ParseSess::with_dcx(dcx, source_map);
    psess.edition = edition;
    psess.attr_id_generator = AttrIdGenerator::at(attr_start);
    (psess, diagnostic)
}
fn clean(psess: &ParseSess, diagnostic: &AtomicBool) -> bool {
    let _ = psess.dcx().emit_stashed_diagnostics();
    !diagnostic.load(Ordering::Relaxed) && psess.dcx().has_errors().is_none()
}
impl<'a> Parser<'a> {
    #[expect(
        clippy::arc_with_non_send_sync,
        reason = "speculative parser state is gathered on the caller thread before owned jobs are submitted"
    )]
    pub(super) fn parse_parallel_bodies(&mut self) -> Option<ast::Crate> {
        if !sync::is_parallel_here()
            || self.body_jobs.is_some()
            || self.capture_cfg
            || self.subparser_name.is_some()
            || self.num_bump_calls != 0
            || !count::cursor_can_parallel_parse(&self.token_cursor)
            || self.psess.dcx().has_errors().is_some()
        {
            return None;
        }
        if let Some(tape) = self.token_cursor.indexed_tape() {
            tape.prepare_parallel_symbols();
        }
        let attr_start = self.psess.attr_id_generator.position();
        let edition = self.psess.edition;
        let original = with_session_globals(|globals| {
            globals.can_share_for_parse().then(|| globals.share_for_parse())
        })?;
        crate::rustc_parse::record_parallel_attempt();
        let (fork, baseline) = original.fork_for_parse();
        let fork = Arc::new(fork);
        let initial = State::copy(self);
        let cut_hint = initial.token_cursor.depth_zero_item_cuts().len();
        let source_map = self.psess.clone_source_map();
        let candidate = with_parse_globals(&fork, || {
            sync::speculate_parse(|| {
                parse_transaction::item_pass(|| {
                    let (psess, diagnostic) = session(source_map, attr_start, edition);
                    let jobs = Arc::new(Lock::new(Vec::with_capacity(cut_hint)));
                    let mut parser = initial.parser(&psess);
                    parser.body_jobs = Some(Arc::clone(&jobs));
                    let (attrs, items, spans) = match parser.parse_mod(exp!(Eof)) {
                        Ok(parts) => parts,
                        Err(diag) => {
                            diag.cancel();
                            return None;
                        }
                    };
                    if !clean(&psess, &diagnostic) {
                        return None;
                    }
                    let mut krate = ast::Crate {
                        attrs,
                        items,
                        spans,
                        id: ast::DUMMY_NODE_ID,
                        is_placeholder: false,
                    };
                    parser.body_jobs = None;
                    let state = State::take(parser);
                    let jobs = match Arc::try_unwrap(jobs) {
                        Ok(jobs) => jobs.into_inner(),
                        Err(_) => return None,
                    };
                    if jobs.len() < 2 {
                        return None;
                    }
                    let input = Arc::new(Input {
                        globals: OwnedParseGlobals::share(&fork)?,
                        source_map: psess.clone_source_map(),
                        jobs,
                        edition,
                    });
                    crate::rustc_parse::record_parallel_body_jobs(input.jobs.len());
                    let output = sync::run_owned_stage(
                        Arc::clone(&input),
                        input.jobs.len(),
                        body_weight,
                        parse_body,
                    );
                    let input = match Arc::try_unwrap(input) {
                        Ok(input) => input,
                        Err(_) => return None,
                    };
                    let mut blocks = FxHashMap::default();
                    let mut effects = Vec::with_capacity(input.jobs.len() * 2 + 1);
                    for (job, body) in input.jobs.into_iter().zip(output) {
                        let body = body?;
                        if blocks.insert(job.ident, body.block).is_some() {
                            return None;
                        }
                        effects.push(job.before);
                        effects.push(body.effects);
                    }
                    let mut splice = Splice { blocks, inconsistent: false };
                    splice.visit_crate(&mut krate);
                    if splice.inconsistent || !splice.blocks.is_empty() {
                        return None;
                    }
                    effects.push(psess.take_parse_effects());
                    Some(Candidate {
                        krate,
                        state,
                        effects,
                        attr_end: psess.attr_id_generator.position(),
                    })
                })
            })
        })
        .flatten();
        let Some(candidate) = candidate else {
            crate::rustc_parse::record_parallel_retry();
            return None;
        };
        // All workers and speculative diagnostics have settled before anything is published.
        if self.psess.attr_id_generator.position() != attr_start
            || !original.commit_parse_spans(&fork, baseline)
        {
            crate::rustc_parse::record_parallel_retry();
            return None;
        }
        assert!(self.psess.attr_id_generator.commit(attr_start, candidate.attr_end));
        for effects in candidate.effects {
            self.psess.append_parse_effects(effects);
        }
        candidate.state.install(self);
        crate::rustc_parse::record_parallel_commit();
        Some(candidate.krate)
    }
    pub(super) fn defer_function_body(
        &mut self,
        ident: Ident,
    ) -> PResult<'a, Option<ast::AttrVec>> {
        let Some(jobs) = self.body_jobs.clone() else { return Ok(None) };
        if self.token != token::OpenBrace || self.capture_cfg {
            return Ok(None);
        }
        let (group_idx, tree, open, close, inside_bumps, attributes) = if let Some(info) =
            self.token_cursor.enclosing_group_info()
        {
            if info.delim != Delimiter::Brace {
                return Ok(None);
            }
            (Some(info.group_idx), None, info.open, info.close, info.inside_bumps, info.attr_count)
        } else {
            if self.token_cursor.can_parallel_parse() {
                sync::retry_parse_serial();
            }
            let tree = self.token_cursor.clone_enclosing_delim();
            let TokenTree::Delimited(spans, _, Delimiter::Brace, stream) = &tree else {
                return Ok(None);
            };
            let (inside_bumps, attributes) = count(stream, true);
            let (open, close) = (spans.open, spans.close);
            (None, Some(tree), open, close, inside_bumps, attributes)
        };
        let source_span = |span: Span| {
            !span.is_dummy()
                && span.ctxt().is_root()
                && !span.from_expansion()
                && span.parent().is_none()
        };
        // A worker may only read compressed spans. Huge bodies stay on the item pass so
        // their span allocations keep the original order relative to surrounding items.
        let Some(bytes) = close.hi().0.checked_sub(open.lo().0) else {
            return Ok(None);
        };
        if bytes > 0x7ffe || !source_span(open) || !source_span(close) {
            return Ok(None);
        }
        let entry_bumps = self.num_bump_calls;
        let end_bumps = entry_bumps
            .checked_add(inside_bumps)
            .and_then(|n| n.checked_add(2))
            .unwrap_or_else(|| sync::retry_parse_serial());
        let lo = self.token.span;
        let maybe_ident = self.prev_token;
        let before_attrs = self.psess.attr_id_generator.position();
        // The first body token can enter a nested group. Keep the body's
        // enclosing cursor so the item pass always jumps to this body's close.
        let mut end_cursor = self.token_cursor.clone();
        self.bump();
        let attrs = self.parse_inner_attributes()?;
        let prefix_bumps = self.num_bump_calls - entry_bumps;
        let prefix_attrs = self.psess.attr_id_generator.position() - before_attrs;
        let remaining_attrs =
            attributes.checked_sub(prefix_attrs).unwrap_or_else(|| sync::retry_parse_serial());
        let attr_start = self
            .psess
            .attr_id_generator
            .reserve(remaining_attrs)
            .unwrap_or_else(|| sync::retry_parse_serial());
        // Detach after inner attributes, preserving the next-token position and
        // bounding the worker at this body's close, even when it is already current.
        let cursor = if let Some(group_idx) = group_idx {
            self.token_cursor
                .detached_for_group(group_idx)
                .unwrap_or_else(|| sync::retry_parse_serial())
        } else {
            let tree = tree.unwrap_or_else(|| sync::retry_parse_serial());
            let mut subtree =
                Parser::new(self.psess, TokenStream::new(vec![tree]), self.subparser_name);
            for _ in 0..prefix_bumps {
                subtree.bump();
            }
            subtree.token_cursor
        };
        let mut state = State::body(self);
        state.token_cursor = cursor;
        state.in_fn_body = true;
        let job = BodyJob {
            ident: ident.span,
            lo,
            maybe_ident,
            state,
            attr_start,
            attr_end: attr_start + remaining_attrs,
            end_bumps,
            before: self.psess.take_parse_effects(),
        };

        if self.token != token::CloseBrace {
            end_cursor.bump_to_end();
            self.token_cursor = end_cursor;
            self.bump();
        }
        if self.token != token::CloseBrace {
            sync::retry_parse_serial();
        }
        self.bump();
        self.num_bump_calls = end_bumps;
        jobs.lock().push(job);
        Ok(Some(attrs))
    }
}

fn body_weight(input: &Input, index: usize) -> u32 {
    let job = &input.jobs[index];
    job.end_bumps.saturating_sub(job.state.num_bump_calls).max(1)
}

fn parse_body(input: &Input, index: usize) -> Option<Body> {
    let job = &input.jobs[index];
    input.globals.enter(|| {
        sync::speculate_parse(|| {
            parse_transaction::body_pass(|| {
                // No `ParseArena` epoch per body: a body writes a KiB or two of nodes, and a
                // fresh 64 KiB chunk for each was most of the parse's allocated bytes. The
                // body's nodes share the thread's current chunk; each holds its own count.
                let (psess, diagnostic) =
                    session(Arc::clone(&input.source_map), job.attr_start, input.edition);
                let mut parser = job.state.clone_body().parser(&psess);
                let result = match parser.maybe_suggest_struct_literal(
                    job.lo,
                    ast::BlockCheckMode::Default,
                    job.maybe_ident,
                ) {
                    Some(result) => result,
                    None => parser.parse_block_tail(
                        job.lo,
                        ast::BlockCheckMode::Default,
                        super::AttemptLocalParseRecovery::Yes,
                    ),
                };
                let block = match result {
                    Ok(block) => block,
                    Err(diag) => {
                        diag.cancel();
                        return None;
                    }
                };
                if parser.token != token::Eof
                    || parser.num_bump_calls != job.end_bumps
                    || !job.state.continuation_matches(&parser)
                    || psess.attr_id_generator.position() != job.attr_end
                    || !clean(&psess, &diagnostic)
                {
                    return None;
                }
                Some(Body { block, effects: psess.take_parse_effects() })
            })
        })
        .flatten()
    })
}

struct Splice {
    blocks: FxHashMap<Span, P<ast::Block>>,
    inconsistent: bool,
}

impl MutVisitor for Splice {
    fn visit_fn(&mut self, mut kind: FnKind<'_>, _: &ast::AttrVec, _: Span, _: ast::NodeId) {
        if let FnKind::Fn(_, _, function) = &mut kind
            && let Some(block) = self.blocks.remove(&function.ident.span)
        {
            if function.body.is_some() {
                self.inconsistent = true;
            } else {
                function.body = Some(block);
            }
        }
        mut_visit::walk_fn(self, kind);
    }
}
