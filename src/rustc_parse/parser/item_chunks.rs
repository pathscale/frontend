//! A file's top-level items, parsed in parallel chunks, with exactly the serial parse's result.
//!
//! `Parser::parse_mod` calls [`Parser::parse_items_in_chunks`] for a whole file (`term` is
//! `Eof`), after it has parsed the file's inner attributes on its own parser and session. Either
//! every item of the file comes back, parsed in chunks and merged, with the parser left where
//! the serial loop would have left it, or `None` comes back and nothing observable has changed:
//! the parser and its `ParseSess` are exactly as they were, and `parse_mod` runs its serial loop.
//! `research/parallel-parse.md` is the long form of this header.
//!
//! # Splitting
//!
//! The file is lexed once, as before, into one frozen `TokenStream`. The depth-0 token trees are
//! walked once with a `TokenCursor` (the parser's own cursor type, cloned from the parser, so
//! every chunk reads the same `Arc`'d stream at the same indices the serial parser reads, and
//! nothing is copied), and a chunk may start at a depth-0 token that
//!
//! - follows a depth-0 `;` token or a depth-0 `{ ... }` group, the two ways an item can end, and
//! - can begin an item: `#` before a `[ ... ]` group, an outer doc comment, one of the keywords
//!   an item can begin with, or a path segment followed by `!` or `::` (a macro call item);
//!
//! and a chunk is cut there only once it spans [`MIN_CHUNK_BYTES`] of source.
//!
//! **The split is a guess the parser checks, never a fact the result relies on.** Chunk `k`'s
//! parser starts on the token its chunk starts on, with everything the serial parser would have
//! at that point (the same cursor over the same stream, the same current and previous token),
//! and runs the serial loop: parse an item, then another. It succeeds only if, after some item,
//! the next token is exactly chunk `k + 1`'s first token. By induction from the first chunk,
//! which starts where the serial loop starts, each chunk then parsed exactly the items the
//! serial loop parses between the same two points, in the same state. A chunk whose items run
//! past its end, stop short of it, or end anywhere else fails, and a failed chunk sends the whole
//! file to the serial loop. A chunk's parser also reads past its end whenever the serial parser
//! would, because its cursor is not cut off there: its lookahead sees the same tokens.
//!
//! # What a chunk writes
//!
//! Each chunk parses against a `ParseSess` of its own (`ParseSess::part`) with a `DiagCtxt` of
//! its own, created inside the chunk's stage item, so no item scope captures it. What the parse
//! leaves in that session (buffered lints, gated spans, ambiguous block spans, and the rest) is
//! the chunk's owned output, merged into the real session in chunk order
//! (`ParseSess::absorb_part`) once every chunk has succeeded.
//!
//! **A chunk that reports anything fails.** Any diagnostic reaching its emitter (an error, a
//! warning, a note), any error or delayed bug counted, any stash, a returned parse error (which
//! is cancelled), or a fatal error (caught): all of it is discarded and the file is parsed
//! serially with the real session, so a file with a diagnostic gives exactly today's output,
//! from the serial parser itself. A clean file gives none, serially or in chunks.
//!
//! # `AttrId`s
//!
//! The one counter the parser takes from is `ParseSess::attr_id_generator`. A chunk takes from
//! its own, from zero, and after each item takes one more id as a marker (the counter has no
//! read), so item `j` of a chunk holds local ids between marker `j - 1` and marker `j`. Once
//! every chunk is back, the real counter is advanced by the ids the serial parse would have
//! taken, one at a time, as the serial parse takes them; chunk `k`'s first id is then known,
//! and every attribute of every item that took an id is renumbered to the id the serial parse
//! gave it. Items that took none are not walked. `NodeId`s need nothing: the parser gives every
//! node `DUMMY_NODE_ID`, and ids are assigned after parsing.
//!
//! # When this runs
//!
//! Only in a parallel session (`is_dyn_thread_safe`, with the `parallel` feature), for a parser
//! over a whole file at depth 0 in its ordinary state (no subparser, no `cfg` capture, recovery
//! allowed, not capturing tokens), with no error already reported, and when the file splits into
//! at least two chunks. A serial session never gets here.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicBool, Ordering};

use thin_vec::ThinVec;

use super::{AllowConstBlockItems, Capturing, ForceCollect, Parser, Recovery};
use crate::exp;
use crate::rustc_ast::mut_visit::{self, MutVisitor};
use crate::rustc_ast::token::{self, Delimiter, IdentIsRaw, Token, TokenKind};
use crate::rustc_ast::tokenstream::{Spacing, TokenCursor, TokenTree};
use crate::rustc_ast::{AttrId, AttrStyle, Attribute, Item};
use crate::rustc_data_structures::sync;
use crate::rustc_errors::emitter::Emitter;
use crate::rustc_errors::{DiagCtxt, DiagCtxtFlags, DiagInner, catch_fatal_errors};
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::source_map::SourceMap;
use crate::rustc_span::{Symbol, kw};

/// The least source a chunk spans before a later item may start the next one.
///
/// **A decision rule, not a tuned number.** A chunk costs its own `ParseSess` and `DiagCtxt`
/// (a handful of empty maps and a lock), one stage item, and its share of the merge, a few
/// microseconds in all; parsing runs at roughly 40 bytes a microsecond (`parse_crate` is about
/// 55 ms over six files of 5,000 to 8,000 lines). At 8 KiB a chunk is a couple of hundred
/// microseconds of parsing, so what a chunk costs stays in the low percents of what it carries,
/// and a file under 16 KiB, most of a crate's files, is never split at all. Chunks are not sized
/// to the session's width: the stage already deals a stage's items out to its threads in runs.
const MIN_CHUNK_BYTES: u32 = 8 * 1024;

/// The keywords an item can begin with (after its attributes). A path segment followed by `!`
/// or `::` (a macro call item) is the other case, checked by [`starts_item`].
const ITEM_KEYWORDS: &[Symbol] = &[
    kw::Async,
    kw::Auto,
    kw::Const,
    kw::Default,
    kw::Enum,
    kw::Extern,
    kw::Fn,
    kw::Impl,
    kw::Macro,
    kw::MacroRules,
    kw::Mod,
    kw::Pub,
    kw::Safe,
    kw::Static,
    kw::Struct,
    kw::Trait,
    kw::Type,
    kw::Union,
    kw::Unsafe,
    kw::Use,
];

/// Where a chunk's parser starts: the state the serial parser has on the chunk's first token.
struct ChunkStart {
    /// Positioned just past `token`, on the file's one stream.
    cursor: TokenCursor,
    token: Token,
    spacing: Spacing,
    /// The last token of the tree before `token`: what the serial parser's `prev_token` is.
    prev: Token,
}

/// What the last chunk's parser holds once it has eaten `Eof`: what the serial parser holds
/// when its loop is done, which the real parser takes over.
struct Tail {
    cursor: TokenCursor,
    token: Token,
    spacing: Spacing,
    prev: Token,
}

/// One chunk's owned output.
struct ChunkOut {
    items: ThinVec<Box<Item>>,
    /// The marker id taken after each item, from the chunk's own counter.
    markers: Vec<u32>,
    /// How many ids the serial parse takes over this chunk: every id the chunk took, less the
    /// markers.
    attr_ids: u32,
    /// The parser's bumps over the chunk, from its first token to the next chunk's (to `Eof`,
    /// and past it, for the last).
    bumps: u32,
    /// For the last chunk.
    tail: Option<Tail>,
    /// The chunk's session, clean, to be absorbed into the real one.
    part: ParseSess,
}

/// The emitter of a chunk's `DiagCtxt`: records that something was emitted, and drops it. The
/// chunk is discarded if it was, and the serial parse emits it for real.
struct ChunkEmitter {
    told: Arc<AtomicBool>,
}

impl Emitter for ChunkEmitter {
    fn emit_diagnostic(&mut self, _diag: DiagInner) {
        self.told.store(true, Ordering::Relaxed);
    }

    fn source_map(&self) -> Option<&SourceMap> {
        None
    }
}

impl<'a> Parser<'a> {
    /// Parse the rest of the file's items in parallel chunks, or return `None` having changed
    /// nothing. See the module header.
    ///
    /// Called with the parser just past the file's inner attributes. On success the parser is
    /// where the serial loop leaves it: `Eof` eaten, `prev_token` the token that tells
    /// `parse_mod` where the module's inner span ends.
    pub(super) fn parse_items_in_chunks(&mut self) -> Option<ThinVec<Box<Item>>> {
        if !cfg!(feature = "parallel") || !sync::is_parallel_here() {
            return None;
        }
        if self.subparser_name.is_some()
            || self.capture_cfg
            || !matches!(self.recovery, Recovery::Allowed)
            || !matches!(self.capture_state.capturing, Capturing::No)
            || self.break_last_token != 0
            || self.token_cursor.depth() != 0
            || self.token == token::Eof
            || self.dcx().has_errors().is_some()
        {
            return None;
        }
        let starts = self.plan_chunks()?;
        let len = starts.len();

        let psess = self.psess;
        let chunks = sync::run_stage(starts, len, move |starts: &Vec<ChunkStart>, index| {
            parse_chunk(psess, starts, index)
        });
        let chunks: Vec<ChunkOut> = chunks.into_iter().collect::<Option<_>>()?;

        // Every chunk is clean. From here on this is the serial parse's result.

        // The ids the serial parse takes over the items, taken from the real counter as it
        // takes them, one at a time; the first is where the items' ids begin.
        let total: u32 = chunks.iter().map(|chunk| chunk.attr_ids).sum();
        let mut next_id = 0;
        if total > 0 {
            next_id = self.psess.attr_id_generator.mk_attr_id().as_u32();
            for _ in 1..total {
                let _ = self.psess.attr_id_generator.mk_attr_id();
            }
        }

        let mut items = ThinVec::with_capacity(chunks.iter().map(|chunk| chunk.items.len()).sum());
        let mut bumps = 0u32;
        let mut tail = None;
        for chunk in chunks {
            let ChunkOut {
                items: chunk_items,
                markers,
                attr_ids,
                bumps: chunk_bumps,
                tail: chunk_tail,
                part,
            } = chunk;
            renumber_attr_ids(&mut items, chunk_items, &markers, next_id);
            next_id += attr_ids;
            bumps += chunk_bumps;
            self.psess.absorb_part(part);
            if chunk_tail.is_some() {
                tail = chunk_tail;
            }
        }

        let Tail { cursor, token, spacing, prev } = tail.expect("the last chunk has a tail");
        self.token_cursor = cursor;
        self.token = token;
        self.token_spacing = spacing;
        self.prev_token = prev;
        self.num_bump_calls += bumps;
        self.expected_token_types.clear();
        Some(items)
    }

    /// Where each chunk starts, or `None` if the file is not worth splitting (fewer than two
    /// chunks) or holds something a lexed file does not (an invisible delimiter).
    ///
    /// One walk over the depth-0 trees with a clone of this parser's cursor, which is at depth 0
    /// just past the current token, the first chunk's first token.
    fn plan_chunks(&self) -> Option<Vec<ChunkStart>> {
        let mut starts = Vec::new();
        starts.push(ChunkStart {
            cursor: self.token_cursor.clone(),
            token: self.token,
            spacing: self.token_spacing,
            prev: self.prev_token,
        });
        let mut chunk_lo = self.token.span.lo();
        let mut cursor = self.token_cursor.clone();
        // The last token of the previous tree, and whether an item can end with that tree.
        let mut prev = self.token;
        let mut prev_ends_item = self.token.kind == token::Semi;

        loop {
            let tree = match cursor.look_ahead(1) {
                None => break,
                Some(&TokenTree::Token(tok, _)) => Tree::Token(tok),
                Some(&TokenTree::Delimited(span, _, delim, _)) => {
                    if matches!(delim, Delimiter::Invisible(_)) {
                        return None;
                    }
                    Tree::Delimited(Token::new(delim.as_close_token_kind(), span.close), delim)
                }
            };
            match tree {
                Tree::Token(tok) => {
                    let cut = prev_ends_item
                        && tok.span.lo().0.saturating_sub(chunk_lo.0) >= MIN_CHUNK_BYTES
                        && starts_item(&tok, cursor.look_ahead(2));
                    let (token, spacing) = cursor.next_and_bump();
                    if cut {
                        starts.push(ChunkStart { cursor: cursor.clone(), token, spacing, prev });
                        chunk_lo = tok.span.lo();
                    }
                    prev = tok;
                    prev_ends_item = tok.kind == token::Semi;
                }
                Tree::Delimited(close, delim) => {
                    // Open, skip the contents, close: three steps whatever the group holds.
                    let depth = cursor.depth();
                    let _open = cursor.next_and_bump();
                    debug_assert_eq!(cursor.depth(), depth + 1);
                    cursor.bump_to_end();
                    let _close = cursor.next_and_bump();
                    debug_assert_eq!(cursor.depth(), depth);
                    prev = close;
                    prev_ends_item = delim == Delimiter::Brace;
                }
            }
        }

        (starts.len() >= 2).then_some(starts)
    }

    /// Chunk `index`'s items, parsed as the serial loop parses them, stopping on the next
    /// chunk's first token (`end`), or at `Eof` for the last chunk. `None` if the items do not
    /// end exactly there or a parse error came back (cancelled; whatever else the parse
    /// reported is in the chunk's `DiagCtxt`, which the caller checks).
    fn parse_chunk_items(&mut self, end: Option<Token>) -> Option<ChunkItems> {
        let mut items: ThinVec<Box<Item>> = ThinVec::new();
        let mut markers = Vec::new();
        loop {
            if let Some(end) = end {
                // Kind and span: the one token of the file that starts the next chunk.
                if self.token == end {
                    break;
                }
                if self.token == token::Eof || self.token.span.lo() > end.span.lo() {
                    return None;
                }
            }
            // As the serial loop: a stray `;` is an error, so the chunk fails.
            while self.maybe_consume_incorrect_semicolon(items.last().map(|x| &**x)) {}
            match self.parse_item(ForceCollect::No, AllowConstBlockItems::Yes) {
                Ok(Some(item)) => {
                    items.push(item);
                    markers.push(self.psess.attr_id_generator.mk_attr_id().as_u32());
                }
                Ok(None) => break,
                Err(err) => {
                    err.cancel();
                    return None;
                }
            }
        }
        let tail = match end {
            Some(end) => {
                if self.token != end {
                    return None;
                }
                None
            }
            None => {
                if !self.eat(exp!(Eof)) {
                    return None;
                }
                Some(Tail {
                    cursor: self.token_cursor.clone(),
                    token: self.token,
                    spacing: self.token_spacing,
                    prev: self.prev_token,
                })
            }
        };
        // Every id the chunk took is below this one. The markers are not the serial parse's.
        let taken = self.psess.attr_id_generator.mk_attr_id().as_u32();
        let attr_ids = taken - markers.len() as u32;
        Some(ChunkItems { items, markers, attr_ids, bumps: self.num_bump_calls, tail })
    }
}

/// What [`Parser::parse_chunk_items`] returns; [`parse_chunk`] adds the session.
struct ChunkItems {
    items: ThinVec<Box<Item>>,
    markers: Vec<u32>,
    attr_ids: u32,
    bumps: u32,
    tail: Option<Tail>,
}

/// A depth-0 tree, as much of it as the walk in `plan_chunks` needs.
enum Tree {
    Token(Token),
    /// The group's close token, and its delimiter.
    Delimited(Token, Delimiter),
}

/// Whether `tok`, followed by `next`, can begin an item. A guess the chunk's parser checks.
fn starts_item(tok: &Token, next: Option<&TokenTree>) -> bool {
    let next_is = |kind: TokenKind| {
        matches!(next, Some(TokenTree::Token(next, _)) if next.kind == kind)
    };
    match tok.kind {
        TokenKind::Pound => {
            matches!(next, Some(TokenTree::Delimited(_, _, Delimiter::Bracket, _)))
        }
        TokenKind::DocComment(_, AttrStyle::Outer, _) => true,
        TokenKind::Ident(name, is_raw) => {
            (is_raw == IdentIsRaw::No && ITEM_KEYWORDS.contains(&name))
                || next_is(TokenKind::Bang)
                || next_is(TokenKind::PathSep)
        }
        _ => false,
    }
}

/// Chunk `index`, run as a stage item: its own session and `DiagCtxt`, created here so that no
/// item scope captures them, its parser at its start, and its items, or `None` if anything at
/// all was reported or the items did not end where the next chunk begins.
fn parse_chunk(psess: &ParseSess, starts: &[ChunkStart], index: usize) -> Option<ChunkOut> {
    let told = Arc::new(AtomicBool::new(false));
    let dcx = DiagCtxt::new(Box::new(ChunkEmitter { told: Arc::clone(&told) }))
        .with_flags(DiagCtxtFlags { can_emit_warnings: true, ..DiagCtxtFlags::default() });
    let part = psess.part(dcx);

    let start = &starts[index];
    let end = starts.get(index + 1).map(|next| next.token);
    let parsed = catch_fatal_errors(|| {
        let mut parser = Parser::with_cursor(&part, start.cursor.clone(), None);
        parser.token = start.token;
        parser.token_spacing = start.spacing;
        parser.prev_token = start.prev;
        parser.parse_chunk_items(end)
    });

    // A stash reaches the emitter (or the error count) only when emitted.
    let _ = part.dcx().emit_stashed_diagnostics();
    let clean = !told.load(Ordering::Relaxed) && part.dcx().has_errors_or_delayed_bugs().is_none();
    match parsed {
        Ok(Some(ChunkItems { items, markers, attr_ids, bumps, tail })) if clean => {
            Some(ChunkOut { items, markers, attr_ids, bumps, tail, part })
        }
        _ => {
            // Discarded: drop what the context holds, so dropping it reports nothing.
            part.dcx().reset_err_count();
            None
        }
    }
}

/// Move `chunk_items` onto `items`, renumbering each attribute's local id to the id the serial
/// parse gave it. `first_id` is the serial id of the chunk's first local id that is not a marker.
///
/// Item `j` holds local ids strictly between marker `j - 1` (or `-1`) and marker `j`; `j`
/// markers precede them, so local id `l` of item `j` is serial id `first_id + l - j`.
fn renumber_attr_ids(
    items: &mut ThinVec<Box<Item>>,
    chunk_items: ThinVec<Box<Item>>,
    markers: &[u32],
    first_id: u32,
) {
    let mut after = 0u32;
    for (j, mut item) in chunk_items.into_iter().enumerate() {
        let marker = markers[j];
        // Ids `after..marker` are this item's.
        if marker > after {
            let mut shift = ShiftAttrIds { lo: after, hi: marker, skipped: j as u32, first_id };
            shift.visit_item(&mut item);
        }
        after = marker + 1;
        items.push(item);
    }
}

/// Renumbers every attribute of one item from the chunk's local ids to the serial ones.
struct ShiftAttrIds {
    /// The item's local ids are `lo..hi`.
    lo: u32,
    hi: u32,
    /// Markers before the item.
    skipped: u32,
    first_id: u32,
}

impl MutVisitor for ShiftAttrIds {
    fn visit_attribute(&mut self, attr: &mut Attribute) {
        let local = attr.id.as_u32();
        debug_assert!(
            (self.lo..self.hi).contains(&local),
            "an attribute outside the item that took its id"
        );
        attr.id = AttrId::from_u32(self.first_id + local - self.skipped);
        mut_visit::walk_attribute(self, attr);
    }
}
