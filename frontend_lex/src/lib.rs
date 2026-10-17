//! The lexer: one forward pass over a source text, to an immutable [`LexedFile`].
//!
//! # What a pass produces
//!
//! Everything a parser and a source map need from the text, so neither reads it again:
//!
//! - **Tokens** ([`Token`], 16 bytes): kind, span, spacing, and a payload that is already final.
//!   An identifier, lifetime, doc comment or literal carries its [`Symbol`], interned during the
//!   pass; a delimiter carries its group. Punctuation is glued at scan time (`<<=` is one token).
//! - **Delimiter groups** ([`Group`]): the token tree as intervals, with nesting depth and the
//!   attribute counts the parallel parser reads.
//! - **The line table**: the offset of every `\n`, and every multi-byte character, for the source
//!   map. Found while scanning whitespace, comments and strings, never by a second pass.
//! - **The symbol gallery**: each identifier symbol's first occurrence, for the confusable lints.
//! - **A lexical error**, if any ([`LexError`]): the first position the input needs a diagnostic
//!   at. A pass that meets one stops there; the diagnostics are reported from it.
//!
//! # Interning
//!
//! The pass interns through an explicit `&Interner`. Within a file, every distinct spelling is
//! hashed and looked up in a table local to the pass ([`table`]): plain memory, no atomics, no
//! lock. Only a spelling the file has not seen before reaches the shared interner, so the
//! shared structure is touched once per distinct name per file, not once per occurrence.
//!
//! # Concurrency
//!
//! None inside a pass: lexing a byte stream is sequential. A [`LexedFile`] is immutable and
//! `Send + Sync`; files lex as independent tasks and every parser task shares the result.

#![no_std]

extern crate alloc;

mod output;
mod prescan;
mod scan;
mod skip;
mod simd;
mod table;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub use frontend_symbol::{Edition, Interner, Symbol};

/// A token's kind. Punctuation is glued: one token per operator, as the parser reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Kind {
    Eq,
    Lt,
    Le,
    EqEq,
    Ne,
    Ge,
    Gt,
    AndAnd,
    OrOr,
    Bang,
    Tilde,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    And,
    Or,
    Shl,
    Shr,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    CaretEq,
    AndEq,
    OrEq,
    ShlEq,
    ShrEq,
    At,
    Dot,
    DotDot,
    DotDotDot,
    DotDotEq,
    Comma,
    Semi,
    Colon,
    PathSep,
    RArrow,
    LArrow,
    FatArrow,
    Pound,
    Dollar,
    Question,
    OpenParen,
    CloseParen,
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    /// `data` is the symbol (normalized to NFC); [`Token::is_raw`] for `r#name`.
    Ident,
    /// `data` is the symbol, apostrophe included, as the parser expects it.
    Lifetime,
    /// `data` is the symbol of the digits; a suffix is in [`LexedFile::suffix`].
    Integer,
    Float,
    /// `data` is the symbol of the contents between the quotes, escapes unprocessed.
    Char,
    Byte,
    Str,
    ByteStr,
    CStr,
    /// `aux` is the number of `#`s.
    StrRaw,
    ByteStrRaw,
    CStrRaw,
    /// `data` is the symbol of the text after `///` or `//!`; `aux` 1 for inner (`//!`).
    LineDoc,
    /// `data` is the symbol of the text between `/**` or `/*!` and `*/`; `aux` 1 for inner.
    BlockDoc,
    Eof,
}

impl Kind {
    /// Punctuation that glues: everything up to `Question`.
    #[inline]
    pub fn is_punct(self) -> bool {
        self <= Kind::Question
    }

    #[inline]
    pub fn is_open_delim(self) -> bool {
        matches!(self, Kind::OpenParen | Kind::OpenBrace | Kind::OpenBracket)
    }

    #[inline]
    pub fn is_close_delim(self) -> bool {
        matches!(self, Kind::CloseParen | Kind::CloseBrace | Kind::CloseBracket)
    }

    #[inline]
    pub fn is_literal(self) -> bool {
        matches!(
            self,
            Kind::Integer
                | Kind::Float
                | Kind::Char
                | Kind::Byte
                | Kind::Str
                | Kind::ByteStr
                | Kind::CStr
                | Kind::StrRaw
                | Kind::ByteStrRaw
                | Kind::CStrRaw
        )
    }
}

/// What follows a token, for the parser's token gluing and pretty printing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spacing {
    /// Whitespace, a comment, or the end of input follows.
    Alone,
    /// Punctuation follows immediately.
    Joint,
    /// Anything else follows immediately.
    JointHidden,
}

const FLAG_JOINT: u8 = 1;
const FLAG_JOINT_HIDDEN: u8 = 2;
const FLAG_RAW: u8 = 4;
const FLAG_SUFFIX: u8 = 8;

/// One token. Offsets are bytes into the source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Token {
    pub lo: u32,
    pub hi: u32,
    /// A symbol's index for identifiers, lifetimes, doc comments and literals; a group index
    /// for delimiters; zero otherwise.
    pub data: u32,
    pub kind: Kind,
    flags: u8,
    /// The `#` count of a raw string; 1 for an inner doc comment.
    pub aux: u8,
    _reserved: u8,
}

const _: () = assert!(core::mem::size_of::<Token>() == 16);

impl Token {
    #[inline]
    pub fn spacing(&self) -> Spacing {
        if self.flags & FLAG_JOINT != 0 {
            Spacing::Joint
        } else if self.flags & FLAG_JOINT_HIDDEN != 0 {
            Spacing::JointHidden
        } else {
            Spacing::Alone
        }
    }

    /// `r#name`.
    #[inline]
    pub fn is_raw(&self) -> bool {
        self.flags & FLAG_RAW != 0
    }

    /// A literal with a suffix, found with [`LexedFile::suffix`].
    #[inline]
    pub fn has_suffix(&self) -> bool {
        self.flags & FLAG_SUFFIX != 0
    }

    /// The payload as a symbol.
    #[inline]
    pub fn symbol(&self) -> Symbol {
        Symbol::new(self.data)
    }
}

/// The delimiter of a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delimiter {
    Parenthesis,
    Brace,
    Bracket,
}

/// Marks "no group": the parent of a top-level group.
pub const NO_GROUP: u32 = u32::MAX;

/// A delimited group: the tokens strictly between `open` and `close`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    pub delim: Delimiter,
    /// Token index of the opening delimiter.
    pub open: u32,
    /// Token index of the closing delimiter.
    pub close: u32,
    /// The enclosing group, or [`NO_GROUP`].
    pub parent: u32,
    /// 1 for a top-level group.
    pub depth: u32,
    /// Outer attributes and doc comments inside the group that belong to its items (not to an
    /// attribute's own brackets, a macro invocation, or a macro definition's body), counted
    /// through nested groups that contribute.
    pub attr_count: u32,
}

/// Why a pass stopped: the first input that needs a diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// A character that starts no token.
    UnknownChar,
    /// An unterminated string, character, block comment or raw string.
    Unterminated,
    /// A malformed literal: bad escape, bad digit, empty exponent, `_` suffix, too many `#`s.
    InvalidLiteral,
    /// A closing delimiter without its opener, or a mismatched or unclosed one.
    Delimiter,
    /// A bidirectional text control codepoint, which is linted.
    TextFlow,
    /// A carriage return inside a doc comment.
    CrInDocComment,
    /// A prefix the edition reserves, a guarded string, a raw lifetime, or a number-led lifetime.
    Reserved,
    /// A frontmatter block, which is validated with its own diagnostics.
    Frontmatter,
}

/// The first position a pass found input needing a diagnostic, and what kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LexError {
    pub pos: u32,
    pub kind: ErrorKind,
}

/// A lexed source text. Immutable; share it with `Arc`.
#[derive(Debug)]
pub struct LexedFile {
    /// The text the offsets index.
    pub src: Arc<String>,
    /// Where lexing began: after a shebang line, if any.
    pub begin: u32,
    /// The tokens, ending in one [`Kind::Eof`] (unless lexing stopped at an error).
    pub tokens: Box<[Token]>,
    /// The groups, in order of their opening delimiters.
    pub groups: Box<[Group]>,
    /// Offsets just past each top-level `}` that closes a group and each top-level `;`: where a
    /// top-level item can end.
    pub item_cuts: Box<[u32]>,
    /// Token indices immediately after top-level item boundaries.
    pub cut_tokens: Box<[u32]>,
    /// Cumulative predicted AttrIds at each corresponding token boundary.
    pub cut_attrs: Box<[u32]>,
    /// Token indices of raw identifiers.
    pub raw_idents: Box<[u32]>,
    /// Literal suffixes, by token index, in token order.
    pub suffixes: Box<[(u32, Symbol)]>,
    /// The offset of every `\n` the pass crossed, ascending.
    pub newlines: Box<[u32]>,
    /// Every multi-byte character the pass crossed: its offset and its length in bytes.
    pub multibyte: Box<[(u32, u8)]>,
    /// Each identifier symbol's first occurrence, in source order: the symbol and the token
    /// index.
    pub gallery: Box<[(Symbol, u32)]>,
    /// The first input that needs a diagnostic; the pass stopped there.
    pub error: Option<LexError>,
}

impl LexedFile {
    /// The suffix of literal token `index`, if it has one.
    pub fn suffix(&self, index: u32) -> Option<Symbol> {
        if !self.tokens[index as usize].has_suffix() {
            return None;
        }
        self.suffixes.binary_search_by_key(&index, |&(i, _)| i).ok().map(|at| self.suffixes[at].1)
    }

    /// Whether `src` was lexed completely and the token tree is balanced.
    pub fn is_clean(&self) -> bool {
        self.error.is_none()
    }
}

/// Options of a pass.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub edition: Edition,
    /// Skip a leading `#!` line that is not an inner attribute.
    pub strip_shebang: bool,
    /// A frontmatter block may open the file (it is reported, see [`ErrorKind::Frontmatter`]).
    pub frontmatter: bool,
}

/// Lexes `src` in one forward pass, interning through `interner`: [`plan`] with one chunk,
/// [`lex_chunk`], [`assemble`].
pub fn lex(src: Arc<String>, options: Options, interner: &Interner) -> Arc<LexedFile> {
    let plan = Plan::single(&src, options);
    let chunk = lex_chunk(&src, &plan, 0);
    assemble(src, &plan, alloc::vec![chunk], interner)
}

/// Where a file is cut into chunks that lex independently (see [`plan`]).
#[derive(Clone, Debug)]
pub struct Plan {
    options: Options,
    /// Where lexing begins: after a shebang line, if any.
    begin: usize,
    /// Chunk boundaries: chunk `i` is `bounds[i]..bounds[i + 1]`.
    bounds: Vec<usize>,
}

impl Plan {
    fn single(src: &str, options: Options) -> Plan {
        let begin = if options.strip_shebang { shebang_len(src).unwrap_or(0) } else { 0 };
        Plan { options, begin, bounds: alloc::vec![begin, src.len()] }
    }

    /// The number of chunks.
    pub fn len(&self) -> usize {
        self.bounds.len() - 1
    }

    /// Chunk boundaries: chunk `i` is `bounds()[i]..bounds()[i + 1]`.
    pub fn bounds(&self) -> &[usize] {
        &self.bounds
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

/// Plans chunks of about `chunk_bytes` each over `src`: a prescan finds where the lexer is
/// between tokens, outside every literal and comment, at the start of a top-level item. A file
/// shorter than two chunks, or one the prescan cannot settle, is one chunk.
pub fn plan(src: &str, options: Options, chunk_bytes: usize) -> Plan {
    let mut plan = Plan::single(src, options);
    let cuts = prescan::plan(src.as_bytes(), plan.begin, chunk_bytes.max(1));
    if !cuts.is_empty() {
        plan.bounds =
            core::iter::once(plan.begin).chain(cuts).chain(core::iter::once(src.len())).collect();
    }
    plan
}

/// Plans chunks of about `chunk_bytes` each over `src` without a prescan: each cut is the first
/// line at or after a grid point that starts, in its first column, with an ASCII letter or `#`,
/// which in Rust source is almost always a top-level item. A guess, checked in [`assemble`]: a
/// chunk that does not leave the lexer fresh (see [`Chunk::continues`]) is lexed again, serially,
/// on to the next boundary where it does. Costs nothing up front; a wrong guess costs about one
/// chunk of serial work, never the result.
pub fn speculate(src: &str, options: Options, chunk_bytes: usize) -> Plan {
    let mut plan = Plan::single(src, options);
    let chunk_bytes = chunk_bytes.max(1);
    let bytes = src.as_bytes();
    let len = bytes.len();
    if len - plan.begin < 2 * chunk_bytes {
        return plan;
    }
    let mut bounds = alloc::vec![plan.begin];
    let mut at = plan.begin + chunk_bytes;
    while at + chunk_bytes / 2 <= len {
        let window = &bytes[at..(at + chunk_bytes).min(len)];
        let found = memchr::memchr_iter(b'\n', window)
            .map(|i| at + i + 1)
            .find(|&line| line < len && (bytes[line].is_ascii_alphabetic() || bytes[line] == b'#'));
        match found {
            Some(line) if len - line >= chunk_bytes / 2 => {
                bounds.push(line);
                at = line + chunk_bytes;
            }
            Some(_) => break,
            None => at += chunk_bytes,
        }
    }
    bounds.push(len);
    plan.bounds = bounds;
    plan
}

/// The segments a parallel prescan of a file is split into (see [`prescan_segment`]).
#[derive(Clone, Debug)]
pub struct Segments {
    options: Options,
    begin: usize,
    /// Segment `i` is `bounds[i]..bounds[i + 1]`.
    bounds: Vec<usize>,
}

impl Segments {
    /// Splits `src` into prescan segments of about `segment_bytes`, at arbitrary bytes: a
    /// segment that turns out to start inside a literal or comment is scanned again when the
    /// segments are composed.
    pub fn new(src: &str, options: Options, segment_bytes: usize) -> Segments {
        let begin = if options.strip_shebang { shebang_len(src).unwrap_or(0) } else { 0 };
        let step = segment_bytes.max(1);
        let mut bounds: Vec<usize> = (begin..src.len()).step_by(step).collect();
        bounds.push(src.len());
        if bounds.len() < 2 {
            bounds = alloc::vec![begin, src.len()];
        }
        Segments { options, begin, bounds }
    }

    pub fn len(&self) -> usize {
        self.bounds.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

/// One prescanned segment. Owned and `Send`: each is scanned by one task.
pub struct PrescanSegment(prescan::Segment);

/// Prescans segment `index`, assuming it starts between tokens. Touches nothing shared.
pub fn prescan_segment(src: &str, segments: &Segments, index: usize) -> PrescanSegment {
    let (start, end) = (segments.bounds[index], segments.bounds[index + 1]);
    PrescanSegment(prescan::segment(src.as_bytes(), start, end))
}

/// The chunk plan from every prescanned segment, in order: cuts of about `chunk_bytes`.
pub fn plan_from_segments(
    src: &str,
    segments: &Segments,
    scanned: Vec<PrescanSegment>,
    chunk_bytes: usize,
) -> Plan {
    let mut plan = Plan::single(src, segments.options);
    if src.len() - segments.begin < 2 * chunk_bytes.max(1) {
        return plan;
    }
    let scanned = scanned.into_iter().map(|PrescanSegment(s)| s).collect();
    let cuts =
        prescan::compose(src.as_bytes(), segments.begin, scanned, &segments.bounds, chunk_bytes);
    if !cuts.is_empty() {
        plan.bounds =
            core::iter::once(plan.begin).chain(cuts).chain(core::iter::once(src.len())).collect();
    }
    plan
}

/// One chunk of a planned file, lexed: tokens and spellings with chunk-local numbering and no
/// symbols. Owned and `Send`; lex each chunk on its own task, then [`assemble`] them in order.
pub struct Chunk(scan::Chunk);

impl Chunk {
    /// Whether the lexer left this chunk fresh: exactly at its end, every group closed, and
    /// none of its last top-level tokens one that changes how a group opened next counts its
    /// attributes. The next chunk of the plan is then valid as lexed. Always true of a
    /// prescanned plan's chunks, but for errors.
    pub fn continues(&self) -> bool {
        self.0.continues
    }

    /// Finds the symbols `interner` already has for this chunk's spellings, adding none: the
    /// part of resolving spellings that runs on the chunk's own task. Optional; [`assemble`]
    /// then interns only the spellings left, in order of first occurrence.
    pub fn look_up(&mut self, src: &str, interner: &Interner) {
        self.0.look_up(src, interner);
    }
}

/// Lexes chunk `index` of `plan`. Touches nothing shared: no interner, no globals.
pub fn lex_chunk(src: &str, plan: &Plan, index: usize) -> Chunk {
    let (start, end) = (plan.bounds[index], plan.bounds[index + 1]);
    let frontmatter = index == 0 && plan.options.frontmatter;
    Chunk(scan::Scanner::lex_range(src, plan.options.edition, start, end, &[], frontmatter))
}

/// Merges `chunks`, every chunk of `plan` in order, into the file, all on the caller's task:
/// [`Assembly::new`], every [`Part::patch`], then [`Assembly::finish`]. To patch on a pool,
/// call those three yourself.
pub fn assemble(
    src: Arc<String>,
    plan: &Plan,
    chunks: Vec<Chunk>,
    interner: &Interner,
) -> Arc<LexedFile> {
    let mut assembly = Assembly::new(src, plan, chunks, interner);
    assembly.parts().into_iter().for_each(Part::patch);
    assembly.finish()
}

/// A file's chunks on their way to a [`LexedFile`], in three phases: [`Assembly::new`] (on one
/// task, in proportion to the distinct spellings), [`Part::patch`] (one owned task per chunk,
/// in proportion to its tokens, writing straight into the file's buffers), and
/// [`Assembly::finish`] (on one task, in proportion to the distinct spellings again).
pub struct Assembly {
    src: Arc<String>,
    begin: usize,
    error: Option<LexError>,
    chunks: Vec<scan::Chunk>,
    symbols: Vec<Vec<u32>>,
    output: Option<Arc<output::Output>>,
}

/// One chunk with what it needs to be rewritten to file numbering and copied into the file.
/// Owned and `Send`.
pub struct Part {
    chunk: scan::Chunk,
    symbols: Vec<u32>,
    at: output::Offsets,
    output: Arc<output::Output>,
}

impl Part {
    /// Rewrites the chunk to file numbering (payloads to symbols, token and group numbers
    /// offset) and copies it to its place in the file's buffers. Touches nothing else shared.
    pub fn patch(mut self) {
        let (tokens, groups) = (self.at.tokens as u32, self.at.groups as u32);
        scan::patch(
            &mut self.chunk,
            &self.symbols,
            tokens,
            groups,
            u32::try_from(self.at.attr_base).expect("attribute prefix overflow"),
        );
        // SAFETY: `at` is this chunk's offsets among the chunks `output` was sized for
        // (`Assembly::parts`), and a part is consumed by its one write.
        unsafe { self.output.write(self.at, &self.chunk) };
    }
}

impl Assembly {
    /// Checks `chunks`, every chunk of `plan` in order, and interns each distinct spelling once,
    /// in order of first occurrence, through `interner`.
    ///
    /// Chunk 0 starts where the serial pass does, and each later chunk is valid if the lexer
    /// was fresh where it starts (see [`Chunk::continues`]). A chunk that does not leave it
    /// fresh is lexed again here, from its own start, past its end to the next boundary where
    /// the lexer is fresh; the chunks it covers are dropped. An error in a chunk with a valid
    /// start is the file's. A prescanned plan never needs the second pass; a speculative one
    /// pays for each wrong guess with one serial chunk or so, not with the rest of the file.
    pub fn new(src: Arc<String>, plan: &Plan, chunks: Vec<Chunk>, interner: &Interner) -> Self {
        assert_eq!(chunks.len(), plan.len(), "assemble takes every chunk of the plan");
        let bounds = &plan.bounds;
        let mut lexed: Vec<Option<scan::Chunk>> =
            chunks.into_iter().map(|Chunk(c)| Some(c)).collect();
        let mut valid = Vec::with_capacity(lexed.len());
        let mut i = 0;
        while i < lexed.len() {
            let mut chunk = lexed[i].take().expect("each chunk is visited once");
            if chunk.error.is_none() && !chunk.continues && i + 1 < lexed.len() {
                chunk = scan::Scanner::lex_range(
                    &src,
                    plan.options.edition,
                    bounds[i],
                    bounds[i + 1],
                    &bounds[i + 2..],
                    i == 0 && plan.options.frontmatter,
                );
            }
            let (reached, failed) = (chunk.end, chunk.error.is_some());
            valid.push(chunk);
            if failed {
                break;
            }
            i = bounds.binary_search(&reached).expect("a chunk ends at a boundary");
        }
        drop(lexed);
        let error = scan::first_error(&valid);
        let symbols =
            if error.is_some() { Vec::new() } else { scan::resolve(&src, &mut valid, interner) };
        Assembly { src, begin: plan.begin, error, chunks: valid, symbols, output: None }
    }

    /// The chunks to patch, in order, each on its own task if the caller likes. Empty for a
    /// file with an error, and for a file of one chunk, which [`Assembly::finish`] patches in
    /// place, without a copy.
    pub fn parts(&mut self) -> Vec<Part> {
        if self.error.is_some() || self.chunks.len() < 2 || self.output.is_some() {
            return Vec::new();
        }
        let (offsets, totals) = output::Offsets::of(&self.chunks);
        let output = Arc::new(output::Output::new(totals, self.chunks.len()));
        self.output = Some(Arc::clone(&output));
        core::mem::take(&mut self.chunks)
            .into_iter()
            .zip(core::mem::take(&mut self.symbols))
            .zip(offsets)
            .map(|((chunk, symbols), at)| Part { chunk, symbols, at, output: Arc::clone(&output) })
            .collect()
    }

    /// The file. Panics if [`Assembly::parts`] gave parts and any was not patched.
    pub fn finish(self) -> Arc<LexedFile> {
        if let Some(error) = self.error {
            return Arc::new(scan::failed(self.src, self.begin, error));
        }
        if let Some(output) = self.output {
            let output = Arc::try_unwrap(output)
                .unwrap_or_else(|_| panic!("every part is patched before the file is finished"));
            return Arc::new(scan::from_filled(self.src, self.begin, output.finish()));
        }
        let mut chunks = self.chunks;
        let mut chunk = chunks.pop().expect("a file has one chunk at least");
        scan::patch(&mut chunk, &self.symbols[0], 0, 0, 0);
        Arc::new(scan::single(self.src, self.begin, chunk))
    }
}

/// The length of a shebang line at the start of `input`, if it has one: `#!` not followed
/// (after whitespace and comments) by `[`, which would make it an inner attribute.
pub fn shebang_len(input: &str) -> Option<usize> {
    scan::shebang_len(input)
}
