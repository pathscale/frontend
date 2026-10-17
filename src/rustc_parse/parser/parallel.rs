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

#[derive(Clone, Copy, Debug)]
pub(super) struct PlannedSegment {
    pub(super) start: u32,
    pub(super) end: u32,
    pub(super) attr_start: u32,
    pub(super) attr_end: u32,
}
pub(super) struct SegmentProgress {
    plans: Vec<PlannedSegment>,
    next: usize,
}

pub(super) fn plan_segments(
    parser: &Parser<'_>,
    pool_width: usize,
    initial_attr: u32,
) -> Vec<PlannedSegment> {
    let Some(tape) = parser.token_cursor.lexed_tape() else { return Vec::new() };
    let cuts = parser.token_cursor.cut_tokens();
    let attrs = parser.token_cursor.cut_attrs();
    if cuts.len() != attrs.len() || cuts.is_empty() {
        return Vec::new();
    }
    let Some(next) = parser.token_cursor.next_index() else { return Vec::new() };
    let Some(mut start) = next.checked_sub(1) else { return Vec::new() };
    let eof = parser.token_cursor.eof_index().unwrap_or(0);
    let first_cut = cuts.partition_point(|&cut| cut <= start);
    let mut eligible = Vec::new();
    for i in first_cut..cuts.len() {
        let end = cuts[i];
        let Some(last) = end.checked_sub(1) else { continue };
        if end == eof {
            eligible.push((end, attrs[i]));
            continue;
        }
        let kind = tape.kind(last as usize);
        if kind == frontend_lex::Kind::Semi {
            eligible.push((end, attrs[i]));
            continue;
        }
        if kind != frontend_lex::Kind::CloseBrace || end >= eof {
            continue;
        }
        let following = tape.kind(end as usize);
        let allowed = match following {
            frontend_lex::Kind::Pound
            | frontend_lex::Kind::LineDoc
            | frontend_lex::Kind::BlockDoc => true,
            frontend_lex::Kind::Ident => {
                let (token, _) = tape.materialize_token(end as usize);
                !token.is_keyword(crate::rustc_span::kw::Else)
                    && !token.is_keyword(crate::rustc_span::kw::Where)
                    && !token.is_keyword(crate::rustc_span::kw::As)
            }
            _ => false,
        };
        if allowed {
            eligible.push((end, attrs[i]));
        }
    }
    let useful = pool_width.saturating_mul(4).max(1);
    let remaining = eof.saturating_sub(start) as usize;
    let target = 256usize.max(remaining.div_ceil(useful));
    let mut output = Vec::new();
    let mut start_attr =
        cuts.iter().position(|&cut| cut == start).map_or(initial_attr, |index| attrs[index]);
    let mut cursor = 0;
    while cursor < eligible.len() {
        let mut selected: Option<(u32, u32)> = None;
        while cursor < eligible.len() {
            let (end, end_attr) = eligible[cursor];
            cursor += 1;
            let count = end.saturating_sub(start) as usize;
            let lo = tape.token(start as usize).lo;
            let hi = tape.token(end.saturating_sub(1) as usize).hi;
            if hi.saturating_sub(lo) > 0x7ffe {
                if let Some((prev_end, prev_attr)) = selected
                    && prev_end.saturating_sub(start) >= 256
                {
                    output.push(PlannedSegment {
                        start,
                        end: prev_end,
                        attr_start: start_attr,
                        attr_end: prev_attr,
                    });
                }
                start = end;
                start_attr = end_attr;
                selected = None;
                break;
            }
            selected = Some((end, end_attr));
            if count >= target {
                break;
            }
        }
        if let Some((end, end_attr)) = selected {
            if end.saturating_sub(start) >= 256 {
                output.push(PlannedSegment {
                    start,
                    end,
                    attr_start: start_attr,
                    attr_end: end_attr,
                });
                start = end;
                start_attr = end_attr;
            }
        }
    }
    output
}
pub(super) struct BodyJob {
    ident: Span,
    lo: Span,
    maybe_ident: Token,
    state: State,
    attr_start: u32,
    attr_end: u32,
    end_bumps: u32,
    terminal: Token,
    eof_span: Span,
    before: ParseEffects,
}
pub(super) struct SegmentJob {
    state: State,
    start: u32,
    end: u32,
    position: u32,
    attr_start: u32,
    attr_end: u32,
    end_bumps: u32,
    terminal: Token,
    start_span: Span,
    eof_span: Span,
    before: ParseEffects,
}
// Keep coordinator metadata out of every Parser snapshot.
pub(super) struct JobQueue {
    jobs: Vec<Job>,
    segment_plan: Option<SegmentProgress>,
}

pub(super) enum Job {
    Body(BodyJob),
    Segment(SegmentJob),
}
struct Input {
    globals: OwnedParseGlobals,
    source_map: Arc<SourceMap>,
    jobs: Vec<Job>,
    edition: Edition,
}
struct Body {
    block: P<ast::Block>,
    effects: ParseEffects,
}
enum Output {
    Body(Body),
    Segment(Segment),
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
    pub(super) fn initialize_segment_plan(&mut self) {
        let Some(jobs) = self.body_jobs.as_ref() else { return };
        if jobs.lock().segment_plan.is_some() || !self.token_cursor.at_file_depth_zero() {
            return;
        }
        let width = sync::parse_pool_width();
        if width <= 1 {
            return;
        }
        let attr = self.psess.attr_id_generator.position();
        let plans = plan_segments(self, width, attr);
        jobs.lock().segment_plan = Some(SegmentProgress { plans, next: 0 });
    }

    pub(super) fn skip_planned_segment(&mut self) -> bool {
        if !self.token_cursor.at_file_depth_zero() || !capture_is_empty(&self.capture_state) {
            return false;
        }
        let Some(next) = self.token_cursor.next_index().and_then(|index| index.checked_sub(1))
        else {
            return false;
        };
        let Some(jobs) = self.body_jobs.clone() else { return false };
        let (plan_index, plan) = {
            let mut queue = jobs.lock();
            let Some(progress) = queue.segment_plan.as_mut() else { return false };
            while progress.next < progress.plans.len() && progress.plans[progress.next].start < next
            {
                progress.next += 1;
            }
            let plan_index = progress.next;
            let Some(plan) = progress.plans.get(plan_index).copied() else { return false };
            (plan_index, plan)
        };
        if plan.start != next || self.num_bump_calls != next {
            return false;
        }
        let Some(count) = plan.attr_end.checked_sub(plan.attr_start) else { return false };
        let before_attr = self.psess.attr_id_generator.position();
        if before_attr != plan.attr_start {
            return false;
        }
        let Some(cursor) = self.token_cursor.detached_for_range(plan.start, plan.end) else {
            sync::retry_parse_serial();
        };
        let Some(tape) = self.token_cursor.lexed_tape() else { return false };
        let Some(last_index) = plan.end.checked_sub(1) else { return false };
        let terminal = tape.materialize_token(last_index as usize).0;
        let start_span = self.token.span;
        let eof_span = tape.materialize_token(last_index as usize).0.span;
        let source_span = |span: Span| {
            !span.is_dummy()
                && span.ctxt().is_root()
                && !span.from_expansion()
                && span.parent().is_none()
        };
        if !source_span(start_span) || !source_span(eof_span) {
            return false;
        }
        let entry_bumps = self.num_bump_calls;
        let end_bumps = entry_bumps
            .checked_add(plan.end - plan.start)
            .unwrap_or_else(|| sync::retry_parse_serial());
        let attr_start = self
            .psess
            .attr_id_generator
            .reserve(count)
            .unwrap_or_else(|| sync::retry_parse_serial());
        let mut state = State::body(self);
        state.token_cursor = cursor;
        let before = self.psess.take_parse_effects();
        jobs.lock().jobs.push(Job::Segment(SegmentJob {
            state,
            start: plan.start,
            end: plan.end,
            position: plan.start,
            attr_start,
            attr_end: attr_start.checked_add(count).unwrap_or_else(|| sync::retry_parse_serial()),
            end_bumps,
            terminal,
            start_span,
            eof_span,
            before,
        }));
        if !self.token_cursor.jump_to(plan.end - 1) {
            sync::retry_parse_serial();
        }
        self.bump();
        self.bump();
        self.num_bump_calls = end_bumps;
        jobs.lock().segment_plan.as_mut().expect("segment plan remains installed").next =
            plan_index + 1;
        true
    }

    #[expect(
        clippy::arc_with_non_send_sync,
        reason = "speculative parser state is gathered on the caller thread before owned jobs are submitted"
    )]
    pub(super) fn parse_parallel_bodies(&mut self) -> Option<ast::Crate> {
        if !sync::is_parallel_here()
            || sync::parse_pool_width() <= 1
            || self.body_jobs.is_some()
            || self.capture_cfg
            || self.subparser_name.is_some()
            || self.num_bump_calls != 0
            || !count::cursor_can_parallel_parse(&self.token_cursor)
            || self.psess.dcx().has_errors().is_some()
        {
            return None;
        }
        if let Some(tape) = self.token_cursor.lexed_tape() {
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
                    let jobs = Arc::new(Lock::new(JobQueue {
                        jobs: Vec::with_capacity(cut_hint),
                        segment_plan: None,
                    }));
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
                        Ok(jobs) => jobs.into_inner().jobs,
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
                    let body_count =
                        input.jobs.iter().filter(|job| matches!(job, Job::Body(_))).count();
                    let segment_count = input.jobs.len() - body_count;
                    crate::rustc_parse::record_parallel_body_jobs(body_count);
                    let (tokens, bytes) = input
                        .jobs
                        .iter()
                        .filter_map(|job| match job {
                            Job::Segment(job) => Some((
                                job.end - job.start,
                                job.eof_span.hi().0 - job.start_span.lo().0,
                            )),
                            Job::Body(_) => None,
                        })
                        .fold((0u64, 0u64), |(t, b), (nt, nb)| (t + nt as u64, b + nb as u64));
                    crate::rustc_parse::record_parallel_segment_jobs(segment_count, tokens, bytes);
                    let output = sync::run_owned_stage(
                        Arc::clone(&input),
                        input.jobs.len(),
                        job_weight,
                        parse_job,
                    );
                    let input = match Arc::try_unwrap(input) {
                        Ok(input) => input,
                        Err(_) => return None,
                    };
                    let mut blocks = FxHashMap::default();
                    let mut effects = Vec::with_capacity(input.jobs.len() * 2 + 1);
                    let mut segments = Vec::new();
                    for (job, result) in input.jobs.into_iter().zip(output) {
                        let result = result?;
                        match (job, result) {
                            (Job::Body(job), Output::Body(body)) => {
                                if blocks.insert(job.ident, body.block).is_some() {
                                    return None;
                                }
                                effects.push(job.before);
                                effects.push(body.effects);
                            }
                            (Job::Segment(job), Output::Segment(segment)) => {
                                effects.push(job.before);
                                let Segment {
                                    position,
                                    start_span,
                                    items,
                                    effects: segment_effects,
                                } = segment;
                                effects.push(segment_effects);
                                segments.push((position, start_span, items));
                            }
                            _ => return None,
                        }
                    }
                    let mut splice = Splice { blocks, inconsistent: false };
                    splice.visit_crate(&mut krate);
                    if splice.inconsistent || !splice.blocks.is_empty() {
                        return None;
                    }
                    segments.sort_by_key(|segment| segment.0);
                    for (_, start_span, segment_items) in segments {
                        let mut items: Vec<_> = krate.items.into_iter().collect();
                        let insertion =
                            items.partition_point(|item| item.span.lo().0 < start_span.lo().0);
                        items.splice(insertion..insertion, segment_items);
                        let mut merged = ast::ArenaVec::new();
                        for item in items {
                            merged.push(item);
                        }
                        krate.items = merged;
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
            terminal: Token::new(token::CloseBrace, close),
            eof_span: close,
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
        jobs.lock().jobs.push(Job::Body(job));
        Ok(Some(attrs))
    }
}

// Capture metadata is local to the coordinator and has no owned merge protocol.
// Candidates that would leave new metadata or closure recovery state retry serially.
fn capture_is_empty(state: &super::CaptureState) -> bool {
    state.parser_replacements.is_empty()
        && state.inner_attr_parser_ranges.is_empty()
        && state.seen_attrs.is_empty()
}
fn job_weight(input: &Input, index: usize) -> u32 {
    match &input.jobs[index] {
        Job::Body(job) => job.end_bumps.saturating_sub(job.state.num_bump_calls).max(1),
        Job::Segment(job) => job.end_bumps.saturating_sub(job.state.num_bump_calls).max(1),
    }
}

fn parse_job(input: &Input, index: usize) -> Option<Output> {
    match &input.jobs[index] {
        Job::Body(job) => parse_body(input, job).map(Output::Body),
        Job::Segment(job) => parse_segment(input, job).map(Output::Segment),
    }
}

fn parse_body(input: &Input, job: &BodyJob) -> Option<Body> {
    let _stage = crate::rustc_data_structures::stage_timing::enter(
        crate::rustc_data_structures::stage_timing::Marker::Parse,
    );
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
                    || parser.token.span != job.eof_span
                    || parser.prev_token != job.terminal
                    || parser.num_bump_calls != job.end_bumps
                    || !job.state.continuation_matches(&parser)
                    || !capture_is_empty(&parser.capture_state)
                    || parser.current_closure != job.state.current_closure
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

struct Segment {
    position: u32,
    start_span: Span,
    items: Vec<P<ast::Item>>,
    effects: ParseEffects,
}

fn parse_segment(input: &Input, job: &SegmentJob) -> Option<Segment> {
    let _stage = crate::rustc_data_structures::stage_timing::enter(
        crate::rustc_data_structures::stage_timing::Marker::Parse,
    );
    input.globals.enter(|| {
        sync::speculate_parse(|| {
            parse_transaction::body_pass(|| {
                let (psess, diagnostic) =
                    session(Arc::clone(&input.source_map), job.attr_start, input.edition);
                let mut parser = job.state.clone().parser(&psess);
                if parser.token.span != job.start_span {
                    return None;
                }
                let result = parser.parse_segment_mod();
                let (attrs, items, _) = match result {
                    Ok(parts) => parts,
                    Err(diag) => {
                        diag.cancel();
                        return None;
                    }
                };
                // Validate the last real token before discarding the synthetic EOF probe.
                // The public module parser consumes its terminator; segment workers leave it
                // current so both terminal-token and logical-bump validation remain exact.
                if !attrs.is_empty()
                    || parser.token != token::Eof
                    || parser.num_bump_calls != job.end_bumps
                    || parser.token.span != job.eof_span
                    || parser.prev_token != job.terminal
                    || parser.token_spacing != crate::rustc_ast::tokenstream::Spacing::Alone
                {
                    return None;
                }
                parser.expected_token_types.clear();
                if !job.state.continuation_matches_segment(&parser)
                    || !capture_is_empty(&parser.capture_state)
                    || psess.attr_id_generator.position() != job.attr_end
                    || !clean(&psess, &diagnostic)
                {
                    return None;
                }
                Some(Segment {
                    position: job.position,
                    start_span: job.start_span,
                    items: items.into_iter().collect(),
                    effects: psess.take_parse_effects(),
                })
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
