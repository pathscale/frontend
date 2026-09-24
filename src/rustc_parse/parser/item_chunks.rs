//! A file's top-level items, parsed in parallel chunks, with exactly the serial parse's result.
//!
//! `Parser::parse_mod` calls [`Parser::parse_items_in_chunks`] for a whole file (`term` is
//! `Eof`), after it has parsed the file's inner attributes on its own parser and session. Either
//! every item of the file comes back, parsed in chunks and merged, with the parser left where
//! the serial loop would have left it, or `None` comes back and nothing observable has changed:
//! the parser and its `ParseSess` are exactly as they were, and `parse_mod` runs its serial loop.
//! `research/parallel-parse.md` is the long form of this header.
//!
//! # Deciding
//!
//! A file shorter than two chunks' worth of source ([`MIN_CHUNK_BYTES`] each) is sent to the
//! serial loop on its extent alone (its first token's start against its last tree's end), before
//! any cursor is cloned or any tree walked. Most of a crate's files end there, at the cost of a
//! few compares.
//!
//! # Splitting
//!
//! The file is lexed once, as before, into one frozen `TokenStream` (`Arc`'d). The depth-0 token
//! trees are walked once with a `TokenCursor` (the parser's own cursor type, cloned from the
//! parser, so every chunk reads the same stream at the same indices the serial parser reads, and
//! nothing is copied or lexed again), and a chunk may start at a depth-0 token that
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
//! from the serial parser itself. Replaying a chunk's diagnostics instead is not provably the
//! serial output: a chunk's parser starts with an empty expected-token set and none of the
//! serial parser's recovery state, and both feed diagnostic text. A clean file gives none,
//! serially or in chunks.
//!
//! # `AttrId`s
//!
//! The one counter the parser takes from is `ParseSess::attr_id_generator`, and a serial id
//! depends on every attribute before it in the file. The planning walk predicts how many ids
//! each chunk takes (one per `#[`, `#![` and doc comment token, not counting the insides of
//! macro calls and `macro_rules!` bodies, which the parser does not parse), and each chunk's
//! counter starts at its predicted serial position. A chunk takes its ids in the serial order,
//! so its ids are the serial ones exactly when every chunk before it took the predicted number.
//! After the parse the real counter is advanced by what the chunks actually took, and a chunk
//! whose start was mispredicted (an earlier chunk took more or fewer) has every attribute in its
//! items shifted by the difference, one walk of that chunk only. On a correct prediction, the
//! usual case, no AST is walked. `NodeId`s need nothing: the parser gives every node
//! `DUMMY_NODE_ID`, and ids are assigned after parsing.
//!
//! # When this runs
//!
//! Only in a parallel session (`is_parallel_here`, with the `parallel` feature), for a parser
//! over a whole file at depth 0 in its ordinary state (no subparser, no `cfg` capture, recovery
//! allowed, not capturing tokens), with no error already reported, at least two chunks' worth of
//! source, and when the file splits into at least two chunks. A serial session never gets here.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicBool, Ordering};

use thin_vec::ThinVec;

use super::{AllowConstBlockItems, Capturing, ForceCollect, Parser, Recovery};
use crate::exp;
use crate::rustc_ast::mut_visit::{self, MutVisitor};
use crate::rustc_ast::token::{self, Delimiter, IdentIsRaw, Token, TokenKind};
use crate::rustc_ast::tokenstream::{Spacing, TokenCursor, TokenStream, TokenTree};
use crate::rustc_ast::{AttrId, AttrStyle, Attribute, Item};
use crate::rustc_data_structures::sync;
use crate::rustc_errors::emitter::Emitter;
use crate::rustc_errors::{DiagCtxt, DiagCtxtFlags, DiagInner, catch_fatal_errors};
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::source_map::SourceMap;
use crate::rustc_span::{Symbol, kw};

/// The least source a chunk spans before a later item may start the next one; a file shorter
/// than twice this is never split.
///
/// **A decision rule, not a tuned number.** A chunk costs its own `ParseSess` and `DiagCtxt`
/// (a handful of empty maps and a lock), one stage item, and its share of the merge, a few
/// microseconds in all; parsing and lexing run at roughly 45 bytes a microsecond (`parse_crate`
/// is 1,288 ms over this crate's 1,424 files, each parsed twice, 57.7 MB). At 8 KiB a chunk is
/// about 180 microseconds of parsing, so what a chunk costs stays in the low percents of what it
/// carries. Chunks are not sized to the session's width: the stage already deals a stage's items
/// out to its threads in runs.
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
    /// The predicted value of the serial `AttrId` counter on `token`: where this chunk's own
    /// counter starts.
    first_attr_id: u32,
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
    /// Where this chunk's counter started (its `ChunkStart::first_attr_id`).
    first_attr_id: u32,
    /// How many ids the chunk took: what the serial parse takes over the same items.
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
        {
            return None;
        }
        // Decided on the file's extent, before any other work: the rest of the file must hold
        // two chunks' worth of source.
        let file_hi = match self.token_cursor.last_tree()? {
            TokenTree::Token(tok, _) => tok.span.hi(),
            TokenTree::Delimited(span, ..) => span.close.hi(),
        };
        if file_hi.0.saturating_sub(self.token.span.lo().0) < 2 * MIN_CHUNK_BYTES
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
        let total =
            chunks.iter().try_fold(0u32, |total, chunk| total.checked_add(chunk.attr_ids))?;

        // Every chunk is clean. From here on this is the serial parse's result.

        // The ids the serial parse takes over the items, taken from the real counter at once;
        // the first is where the first chunk's ids begin.
        let mut serial_first = self.psess.attr_id_generator.take(total);

        let mut items = ThinVec::with_capacity(chunks.iter().map(|chunk| chunk.items.len()).sum());
        let mut bumps = 0u32;
        let mut tail = None;
        for chunk in chunks {
            let ChunkOut {
                items: mut chunk_items,
                first_attr_id,
                attr_ids,
                bumps: chunk_bumps,
                tail: chunk_tail,
                part,
            } = chunk;
            // The chunk's ids run from `first_attr_id`, the serial ones from `serial_first`, in
            // the same order; they differ only if an earlier chunk's count was mispredicted.
            let delta = serial_first.wrapping_sub(first_attr_id);
            if delta != 0 && attr_ids != 0 {
                let mut shift = ShiftAttrIds { delta };
                for item in chunk_items.iter_mut() {
                    shift.visit_item(item);
                }
            }
            items.extend(chunk_items);
            serial_first += attr_ids;
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

    /// Where each chunk starts, with its predicted first `AttrId`, or `None` if the file is not
    /// worth splitting (fewer than two chunks) or holds something a lexed file does not (an
    /// invisible delimiter).
    ///
    /// One walk over the depth-0 trees with a clone of this parser's cursor, which is at depth 0
    /// just past the current token, the first chunk's first token. A group is skipped in three
    /// cursor steps; its attribute tokens are counted by a flat scan of its trees.
    fn plan_chunks(&self) -> Option<Vec<ChunkStart>> {
        let mut ids = self.psess.attr_id_generator.peek();
        let mut starts = Vec::new();
        starts.push(ChunkStart {
            cursor: self.token_cursor.clone(),
            token: self.token,
            spacing: self.token_spacing,
            prev: self.prev_token,
            first_attr_id: ids,
        });
        let mut chunk_lo = self.token.span.lo();
        let mut cursor = self.token_cursor.clone();
        let mut recent = Recent::new();
        if takes_attr_id(&self.token, cursor.look_ahead(1), cursor.look_ahead(2)) {
            ids = ids.checked_add(1)?;
        }
        recent.token(&self.token);
        // The last token of the previous tree, and whether an item can end with that tree.
        let mut prev = self.token;
        let mut prev_ends_item = self.token.kind == token::Semi;

        loop {
            let tree = match cursor.look_ahead(1) {
                None => break,
                Some(&TokenTree::Token(tok, _)) => Tree::Token(tok),
                Some(TokenTree::Delimited(span, _, delim, inner)) => {
                    if matches!(delim, Delimiter::Invisible(_)) {
                        return None;
                    }
                    let inside = if recent.macro_body_next() { 0 } else { count_attr_ids(inner)? };
                    let close = Token::new(delim.as_close_token_kind(), span.close);
                    Tree::Delimited(close, *delim, inside)
                }
            };
            match tree {
                Tree::Token(tok) => {
                    let cut = prev_ends_item
                        && tok.span.lo().0.saturating_sub(chunk_lo.0) >= MIN_CHUNK_BYTES
                        && starts_item(&tok, cursor.look_ahead(2));
                    let takes = takes_attr_id(&tok, cursor.look_ahead(2), cursor.look_ahead(3));
                    let (token, spacing) = cursor.next_and_bump();
                    if cut {
                        starts.push(ChunkStart {
                            cursor: cursor.clone(),
                            token,
                            spacing,
                            prev,
                            first_attr_id: ids,
                        });
                        chunk_lo = tok.span.lo();
                    }
                    if takes {
                        ids = ids.checked_add(1)?;
                    }
                    recent.token(&tok);
                    prev = tok;
                    prev_ends_item = tok.kind == token::Semi;
                }
                Tree::Delimited(close, delim, inside) => {
                    // Open, skip the contents, close: three steps whatever the group holds.
                    let depth = cursor.depth();
                    let _open = cursor.next_and_bump();
                    debug_assert_eq!(cursor.depth(), depth + 1);
                    cursor.bump_to_end();
                    let _close = cursor.next_and_bump();
                    debug_assert_eq!(cursor.depth(), depth);
                    ids = ids.checked_add(inside)?;
                    recent.other();
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
                Ok(Some(item)) => items.push(item),
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
        Some(ChunkItems { items, bumps: self.num_bump_calls, tail })
    }
}

/// What [`Parser::parse_chunk_items`] returns; [`parse_chunk`] adds the ids and the session.
struct ChunkItems {
    items: ThinVec<Box<Item>>,
    bumps: u32,
    tail: Option<Tail>,
}

/// A depth-0 tree, as much of it as the walk in `plan_chunks` needs.
enum Tree {
    Token(Token),
    /// The group's close token, its delimiter, and the ids predicted inside it.
    Delimited(Token, Delimiter, u32),
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

/// Whether the parser takes an `AttrId` for `tok`, followed by `next` and `after`: a doc
/// comment, `#[`, or `#![`. A prediction the merge checks.
fn takes_attr_id(tok: &Token, next: Option<&TokenTree>, after: Option<&TokenTree>) -> bool {
    let is_bracket =
        |tree: Option<&TokenTree>| matches!(tree, Some(TokenTree::Delimited(_, _, Delimiter::Bracket, _)));
    match tok.kind {
        TokenKind::DocComment(..) => true,
        TokenKind::Pound => {
            is_bracket(next)
                || (matches!(next, Some(TokenTree::Token(bang, _)) if bang.kind == TokenKind::Bang)
                    && is_bracket(after))
        }
        _ => false,
    }
}

/// The ids the parser is predicted to take inside `stream`, a group's trees, or `None` if the
/// count overflows. A flat scan with an explicit stack (no recursion, however deep the nesting),
/// which does not look inside a group that is a macro call's input (`m!(..)`, `m![..]`,
/// `m! {..}`) or a `macro_rules!` body: the parser does not parse those.
fn count_attr_ids(stream: &TokenStream) -> Option<u32> {
    let mut count = 0u32;
    let mut trees = stream;
    let mut index = 0usize;
    let mut recent = Recent::new();
    let mut outer: Vec<(&TokenStream, usize, Recent)> = Vec::new();
    loop {
        let Some(tree) = trees.get(index) else {
            match outer.pop() {
                Some((up, at, was)) => {
                    trees = up;
                    index = at;
                    recent = was;
                    continue;
                }
                None => return Some(count),
            }
        };
        index += 1;
        match tree {
            TokenTree::Token(tok, _) => {
                if takes_attr_id(tok, trees.get(index), trees.get(index + 1)) {
                    count = count.checked_add(1)?;
                }
                recent.token(tok);
            }
            TokenTree::Delimited(_, _, _, inner) => {
                let skip = recent.macro_body_next();
                recent.other();
                if !skip {
                    outer.push((trees, index, recent));
                    trees = inner;
                    index = 0;
                    recent = Recent::new();
                }
            }
        }
    }
}

/// What a token was, as far as [`Recent::macro_body_next`] needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    Other,
    Bang,
    MacroRules,
    Ident,
}

/// The last three trees of one stream, latest first.
#[derive(Clone, Copy)]
struct Recent([Seen; 3]);

impl Recent {
    fn new() -> Self {
        Recent([Seen::Other; 3])
    }

    fn push(&mut self, seen: Seen) {
        self.0 = [seen, self.0[0], self.0[1]];
    }

    fn token(&mut self, tok: &Token) {
        self.push(match tok.kind {
            TokenKind::Bang => Seen::Bang,
            TokenKind::Ident(name, IdentIsRaw::No) if name == kw::MacroRules => Seen::MacroRules,
            TokenKind::Ident(..) => Seen::Ident,
            _ => Seen::Other,
        });
    }

    fn other(&mut self) {
        self.push(Seen::Other);
    }

    /// Whether a group that comes next is a macro call's input (after `!`) or a `macro_rules!`
    /// body (after `macro_rules ! name`). A negation of a parenthesised expression (`!(..)`) is
    /// taken for a macro input too; an attribute inside one is then not predicted, which the
    /// merge corrects.
    fn macro_body_next(&self) -> bool {
        matches!(
            self.0,
            [Seen::Bang, ..] | [Seen::Ident | Seen::MacroRules, Seen::Bang, Seen::MacroRules]
        )
    }
}

/// Chunk `index`, run as a stage item: its own session and `DiagCtxt`, created here so that no
/// item scope captures them, its parser at its start, and its items, or `None` if anything at
/// all was reported or the items did not end where the next chunk begins.
fn parse_chunk(psess: &ParseSess, starts: &[ChunkStart], index: usize) -> Option<ChunkOut> {
    let told = Arc::new(AtomicBool::new(false));
    let dcx = DiagCtxt::new(Box::new(ChunkEmitter { told: Arc::clone(&told) }))
        .with_flags(DiagCtxtFlags { can_emit_warnings: true, ..DiagCtxtFlags::default() });
    let start = &starts[index];
    let part = psess.part(dcx, start.first_attr_id);

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
        Ok(Some(ChunkItems { items, bumps, tail })) if clean => {
            let attr_ids = part.attr_id_generator.peek().wrapping_sub(start.first_attr_id);
            Some(ChunkOut {
                items,
                first_attr_id: start.first_attr_id,
                attr_ids,
                bumps,
                tail,
                part,
            })
        }
        _ => {
            // Discarded: drop what the context holds, so dropping it reports nothing.
            part.dcx().reset_err_count();
            None
        }
    }
}

/// Moves every attribute id of a chunk's items by `delta` (wrapping, so a negative difference
/// is its two's complement): from where the chunk's counter started to where the serial one is.
struct ShiftAttrIds {
    delta: u32,
}

impl MutVisitor for ShiftAttrIds {
    fn visit_attribute(&mut self, attr: &mut Attribute) {
        attr.id = AttrId::from_u32(attr.id.as_u32().wrapping_add(self.delta));
        mut_visit::walk_attribute(self, attr);
    }
}
