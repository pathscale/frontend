//! The pass itself: one forward walk over the bytes, dispatching on each token's first byte.
//!
//! Rules are `rustc_lexer`'s, token for token; where a rule produces something the parser
//! reports (an unterminated literal, a bad escape, a reserved prefix, an unknown character, a
//! mismatched delimiter, a bidi codepoint), the pass records a [`LexError`] there and stops.
//!
//! Positions are byte offsets into the text and always lie on character boundaries: every
//! non-ASCII character is stepped over whole.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use frontend_symbol::{Edition, Interner, Symbol};
use rustc_literal_escaper::{Mode, check_for_errors};
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};
use unicode_properties::UnicodeEmoji;

use crate::output::Filled;
use crate::table::{Probe, Table};
use crate::{
    Delimiter, ErrorKind, FLAG_JOINT, FLAG_JOINT_HIDDEN, FLAG_RAW, FLAG_SUFFIX, Group, Kind,
    LexError, LexedFile, NO_GROUP, Token,
};

/// ASCII identifier start: letters and `_`.
const ID_START: u8 = 1;
/// ASCII identifier continue: letters, digits and `_`.
const ID_CONT: u8 = 2;

/// Byte classes, for the ASCII half of the identifier rules. Non-ASCII bytes are classless and
/// send the scan to the Unicode rules.
static CLASS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut b = 0;
    while b < 128 {
        let c = b as u8;
        if c.is_ascii_alphabetic() || c == b'_' {
            table[b] = ID_START | ID_CONT;
        } else if c.is_ascii_digit() {
            table[b] = ID_CONT;
        }
        b += 1;
    }
    table
};

#[inline(always)]
fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ')
}

/// `Pattern_White_Space`, the whitespace of the Rust grammar.
#[inline]
fn is_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{0085}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

#[inline]
fn is_id_start(c: char) -> bool {
    c == '_' || unicode_ident::is_xid_start(c)
}

#[inline]
fn is_id_continue(c: char) -> bool {
    unicode_ident::is_xid_continue(c)
}

/// U+202A..U+202E and U+2066..U+2069: bidirectional text controls, which are linted.
#[inline]
fn is_text_flow(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// A comment opener at `rest`: `Some(true)` for a doc comment, `Some(false)` for a plain one.
#[inline]
fn comment_at(rest: &[u8]) -> Option<bool> {
    match rest {
        [b'/', b'/', b'!', ..] => Some(true),
        [b'/', b'/', b'/', b'/', ..] => Some(false),
        [b'/', b'/', b'/', ..] => Some(true),
        [b'/', b'/', ..] => Some(false),
        [b'/', b'*', b'!', ..] => Some(true),
        [b'/', b'*', b'*', b'*' | b'/', ..] => Some(false),
        [b'/', b'*', b'*', ..] => Some(true),
        [b'/', b'*', ..] => Some(false),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Leaf {
    kind: Kind,
    macro_rules: bool,
    decl_macro: bool,
}

/// The tokens of one group being scanned, for attribute counting.
struct Frame {
    group: u32,
    attributes: u32,
    /// The latest three token trees of this group, newest first; `None` is a nested group.
    previous: [Option<Leaf>; 3],
    contributes: bool,
}

impl Frame {
    fn new(group: u32, contributes: bool) -> Frame {
        Frame { group, attributes: 0, previous: [None; 3], contributes }
    }

    #[inline]
    fn remember(&mut self, leaf: Option<Leaf>) {
        self.previous = [leaf, self.previous[0], self.previous[1]];
    }
}

/// A distinct spelling of the file: its first occurrence and its hash, kept so resolving it
/// does not hash it again.
struct Atom {
    lo: u32,
    len: u32,
    hash: u64,
    flags: u8,
}

/// The atom is a non-ASCII identifier or lifetime: its symbol is its NFC normalization.
const ATOM_NFC: u8 = 1;
/// The atom has been an identifier token (gallery bookkeeping).
const ATOM_IDENT_SEEN: u8 = 2;
/// The atom spells `macro_rules`.
const ATOM_MACRO_RULES: u8 = 4;
/// The atom spells `macro`.
const ATOM_MACRO: u8 = 8;

/// Stop the pass: the input needs a diagnostic at a position already recorded.
struct Stop;

type Step = Result<usize, Stop>;

pub(crate) struct Scanner<'a> {
    text: &'a str,
    src: &'a [u8],
    edition: Edition,
    /// One past the last byte this scanner owns: its chunk's end.
    end: usize,
    /// Later boundaries the chunk may extend to (see [`Scanner::lex_range`]).
    ahead: &'a [usize],
    /// Whether the chunk ends the file, and so ends in an EOF token.
    last: bool,
    table: Table,
    tokens: Vec<Token>,
    groups: Vec<Group>,
    frames: Vec<Frame>,
    item_cuts: Vec<u32>,
    raw_idents: Vec<u32>,
    /// Literal suffixes as (token, atom).
    suffixes: Vec<(u32, u32)>,
    newlines: Vec<u32>,
    multibyte: Vec<(u32, u8)>,
    /// Spellings by atom, in order of first occurrence.
    atoms: Vec<Atom>,
    /// Each atom's first identifier token, in token order.
    gallery_candidates: Vec<(u32, u32)>,
    error: Option<LexError>,
    /// Where the loop stopped: at or past `end`, once the last token is done.
    stop: usize,
}

/// One chunk, lexed: everything a [`LexedFile`] holds, with chunk-local token, group and atom
/// numbers, and no symbols. Owned and `Send`: a chunk is lexed by one task and handed to
/// [`assemble`].
pub(crate) struct Chunk {
    /// Each atom's symbol if the interner had it when looked up, else `UNKNOWN`; empty if not
    /// looked up (see [`Chunk::look_up`]).
    pub(crate) known: Vec<u32>,
    /// One past the last byte lexed: the chunk's end, or a later boundary it extended to.
    pub(crate) end: usize,
    pub(crate) tokens: Vec<Token>,
    pub(crate) groups: Vec<Group>,
    pub(crate) item_cuts: Vec<u32>,
    pub(crate) raw_idents: Vec<u32>,
    pub(crate) suffixes: Vec<(u32, u32)>,
    pub(crate) newlines: Vec<u32>,
    pub(crate) multibyte: Vec<(u32, u8)>,
    atoms: Vec<Atom>,
    pub(crate) gallery_candidates: Vec<(u32, u32)>,
    pub(crate) error: Option<LexError>,
    /// Whether the lexer leaves this chunk in the state a chunk starts in: between tokens at
    /// exactly its end, every group closed, and no token at its end that changes how a
    /// group opened just after it counts attributes. A chunk that follows one that does lexes
    /// exactly as the serial pass would.
    pub(crate) continues: bool,
}

impl<'a> Scanner<'a> {
    /// Lexes `text[start..end]`, which begins between tokens outside any literal or comment.
    /// `last` when the range ends the file. Touches nothing shared: no interner, no globals.
    ///
    /// `ahead` are later chunk boundaries, ascending, the last of them the end of the file: if
    /// the lexer does not reach `end` in a chunk's starting state, it carries on to the next
    /// of them, until it does or the file ends. [`Chunk::end`] says where it stopped.
    pub(crate) fn lex_range(
        text: &'a str,
        edition: Edition,
        start: usize,
        end: usize,
        ahead: &'a [usize],
        frontmatter: bool,
    ) -> Chunk {
        let len = end - start;
        let last = end == text.len();
        let mut s = Scanner {
            text,
            src: text.as_bytes(),
            edition,
            end,
            ahead,
            last,
            table: Table::for_source(len),
            tokens: Vec::with_capacity(len / 4 + 16),
            groups: Vec::with_capacity(len / 32 + 8),
            frames: alloc::vec![Frame::new(NO_GROUP, false)],
            item_cuts: Vec::new(),
            raw_idents: Vec::new(),
            suffixes: Vec::new(),
            newlines: Vec::with_capacity(len / 32 + 8),
            multibyte: Vec::new(),
            atoms: Vec::with_capacity(len / 64 + 16),
            gallery_candidates: Vec::new(),
            error: None,
            stop: usize::MAX,
        };
        if frontmatter && s.frontmatter_at(start) {
            let _ = s.fail::<()>(start, ErrorKind::Frontmatter);
        } else {
            let _ = s.lex(start);
        }
        let continues = s.error.is_none() && s.fresh_at(s.stop);
        Chunk {
            end: s.end,
            known: Vec::new(),
            tokens: s.tokens,
            groups: s.groups,
            item_cuts: s.item_cuts,
            raw_idents: s.raw_idents,
            suffixes: s.suffixes,
            newlines: s.newlines,
            multibyte: s.multibyte,
            atoms: s.atoms,
            gallery_candidates: s.gallery_candidates,
            error: s.error,
            continues,
        }
    }

    // ---- The loop. ----

    fn lex(&mut self, mut pos: usize) -> Result<(), Stop> {
        let src = self.src;
        let mut end = self.end;
        loop {
            if pos >= end {
                if !self.fresh_at(pos)
                    && let Some(next) = self.extend(pos)
                {
                    end = next;
                    continue;
                }
                return self.finish(pos);
            }
            let b = src[pos];
            pos = match b {
                b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r' => self.whitespace(pos),
                b'r' => self.r_prefixed(pos)?,
                b'b' => self.b_prefixed(pos)?,
                b'c' => self.c_prefixed(pos)?,
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.ident(pos)?,
                b'0'..=b'9' => self.number(pos)?,
                b'"' => self.string(pos, pos + 1, Kind::Str, Mode::Str)?,
                b'\'' => self.lifetime_or_char(pos)?,
                b'/' => match src.get(pos + 1) {
                    Some(b'/') => self.line_comment(pos)?,
                    Some(b'*') => self.block_comment(pos)?,
                    _ => self.punct(pos),
                },
                b'(' => self.open(pos, Delimiter::Parenthesis, Kind::OpenParen),
                b'[' => self.open(pos, Delimiter::Bracket, Kind::OpenBracket),
                b'{' => self.open(pos, Delimiter::Brace, Kind::OpenBrace),
                b')' => self.close(pos, Delimiter::Parenthesis, Kind::CloseParen)?,
                b']' => self.close(pos, Delimiter::Bracket, Kind::CloseBracket)?,
                b'}' => self.close(pos, Delimiter::Brace, Kind::CloseBrace)?,
                b'#' if matches!(src.get(pos + 1), Some(b'"' | b'#')) => {
                    // A guarded string prefix: reserved syntax.
                    return self.fail(pos, ErrorKind::Reserved);
                }
                b'=' | b'<' | b'>' | b'!' | b'~' | b'+' | b'-' | b'*' | b'%' | b'^' | b'&'
                | b'|' | b'@' | b'.' | b',' | b';' | b':' | b'#' | b'$' | b'?' => self.punct(pos),
                0x80.. => self.non_ascii(pos)?,
                _ => return self.fail(pos, ErrorKind::UnknownChar),
            };
        }
    }

    /// Whether the lexer at `pos` is in the state a chunk starts in: exactly at the chunk's end,
    /// every group closed, and the latest three top-level token trees such that a group opened
    /// in the next chunk counts attributes as it would after none (see `open`, which reads
    /// back three trees). A chunk that starts where its predecessor is fresh lexes exactly as
    /// the serial pass would.
    #[cold]
    fn fresh_at(&self, pos: usize) -> bool {
        if pos != self.end || self.frames.len() != 1 {
            return false;
        }
        let [newest, second, third] = self.frames[0].previous;
        let has = |leaf: Option<Leaf>, f: fn(Leaf) -> bool| leaf.is_some_and(f);
        // The newest is read as `prev` by an opener first in the chunk, as `prev2` by one
        // second, and as `prev3` by one third; the older two only by openers before any three
        // new trees, and only as `prev2`/`prev3`.
        !has(newest, |l| {
            matches!(l.kind, Kind::Pound | Kind::Bang) || l.macro_rules || l.decl_macro
        }) && !has(second, |l| l.decl_macro)
            && !has(third, |l| l.decl_macro)
            && !(has(second, |l| l.kind == Kind::Bang) && has(third, |l| l.macro_rules))
    }

    /// Moves the chunk's end to the next boundary in `ahead` past `pos`, if there is one.
    #[cold]
    fn extend(&mut self, pos: usize) -> Option<usize> {
        let skip = self.ahead.iter().position(|&bound| bound > pos)?;
        let next = self.ahead[skip];
        self.ahead = &self.ahead[skip + 1..];
        self.end = next;
        self.last = next == self.src.len();
        Some(next)
    }

    fn finish(&mut self, end: usize) -> Result<(), Stop> {
        self.stop = end;
        if !self.last {
            // Unbalanced or not, a chunk that does not end the file is not judged here: if it
            // is not fresh at its end, the next chunk is not valid and this one is lexed again
            // past its end (see `continues`).
            return Ok(());
        }
        if self.frames.len() != 1 {
            return self.fail(end, ErrorKind::Delimiter);
        }
        self.tokens.push(Token {
            lo: end as u32,
            hi: end as u32,
            data: 0,
            kind: Kind::Eof,
            flags: 0,
            aux: 0,
            _reserved: 0,
        });
        Ok(())
    }

    #[cold]
    fn fail<T>(&mut self, pos: usize, kind: ErrorKind) -> Result<T, Stop> {
        self.error = Some(LexError { pos: pos as u32, kind });
        Err(Stop)
    }

    // ---- Emission. ----

    /// The spacing flags of a token ending at `hi`, from the byte after it.
    #[inline(always)]
    fn spacing_at(&self, hi: usize) -> u8 {
        let src = self.src;
        let Some(&b) = src.get(hi) else { return 0 };
        match b {
            b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ' => 0,
            b'/' => match comment_at(&src[hi..]) {
                Some(true) => FLAG_JOINT_HIDDEN,
                Some(false) => 0,
                None => FLAG_JOINT,
            },
            b'=' | b'<' | b'>' | b'!' | b'~' | b'+' | b'-' | b'*' | b'%' | b'^' | b'&' | b'|'
            | b'@' | b'.' | b',' | b';' | b':' | b'#' | b'$' | b'?' => FLAG_JOINT,
            0xC2 if src.get(hi + 1) == Some(&0x85) => 0,
            0xE2 if matches!(&src[hi + 1..], [0x80, 0x8E | 0x8F | 0xA8 | 0xA9, ..]) => 0,
            _ => FLAG_JOINT_HIDDEN,
        }
    }

    /// Pushes a token that is not a delimiter, keeping the group's attribute bookkeeping.
    #[inline(always)]
    fn leaf(&mut self, kind: Kind, lo: usize, hi: usize, data: u32, flags: u8, aux: u8) -> u32 {
        let index = self.tokens.len() as u32;
        let flags = flags | self.spacing_at(hi);
        self.tokens.push(Token {
            lo: lo as u32,
            hi: hi as u32,
            data,
            kind,
            flags,
            aux,
            _reserved: 0,
        });
        let frame = self.frames.last_mut().expect("the root frame is never popped");
        // An identifier's payload is still its atom here.
        let (macro_rules, decl_macro) = if kind == Kind::Ident {
            let atom = self.atoms[data as usize].flags;
            (atom & ATOM_MACRO_RULES != 0, flags & FLAG_RAW == 0 && atom & ATOM_MACRO != 0)
        } else {
            (false, false)
        };
        if matches!(kind, Kind::LineDoc | Kind::BlockDoc) {
            frame.attributes += 1;
        }
        frame.remember(Some(Leaf { kind, macro_rules, decl_macro }));
        if kind == Kind::Semi && frame.group == NO_GROUP {
            self.item_cuts.push(hi as u32);
        }
        index
    }

    fn open(&mut self, lo: usize, delim: Delimiter, kind: Kind) -> usize {
        let index = self.tokens.len() as u32;
        let group = self.groups.len() as u32;
        let frame = self.frames.last_mut().expect("the root frame is never popped");
        let parent = frame.group;
        let [prev, prev2, prev3] = frame.previous;
        let is = |leaf: Option<Leaf>, kind| leaf.is_some_and(|leaf| leaf.kind == kind);
        let attribute = delim == Delimiter::Bracket
            && (is(prev, Kind::Pound) || is(prev, Kind::Bang) && is(prev2, Kind::Pound));
        let invocation = is(prev, Kind::Bang);
        let macro_rules = is(prev2, Kind::Bang) && prev3.is_some_and(|leaf| leaf.macro_rules);
        let decl_macro =
            prev2.is_some_and(|leaf| leaf.decl_macro) || prev3.is_some_and(|leaf| leaf.decl_macro);
        frame.attributes += u32::from(attribute);
        frame.remember(None);
        let depth = if parent == NO_GROUP { 1 } else { self.groups[parent as usize].depth + 1 };
        self.groups.push(Group { delim, open: index, close: 0, parent, depth, attr_count: 0 });
        self.frames
            .push(Frame::new(group, !attribute && !invocation && !macro_rules && !decl_macro));
        let flags = self.spacing_at(lo + 1);
        self.tokens.push(Token {
            lo: lo as u32,
            hi: lo as u32 + 1,
            data: group,
            kind,
            flags,
            aux: 0,
            _reserved: 0,
        });
        lo + 1
    }

    fn close(&mut self, lo: usize, delim: Delimiter, kind: Kind) -> Step {
        if self.frames.len() == 1 {
            return self.fail(lo, ErrorKind::Delimiter);
        }
        let frame = self.frames.pop().expect("checked above");
        let group = frame.group;
        if self.groups[group as usize].delim != delim {
            return self.fail(lo, ErrorKind::Delimiter);
        }
        let index = self.tokens.len() as u32;
        let g = &mut self.groups[group as usize];
        g.close = index;
        g.attr_count = frame.attributes;
        let top_brace = g.parent == NO_GROUP && delim == Delimiter::Brace;
        if frame.contributes {
            let outer = self.frames.last_mut().expect("the root frame is never popped");
            outer.attributes += frame.attributes;
        }
        if top_brace {
            self.item_cuts.push(lo as u32 + 1);
        }
        let flags = self.spacing_at(lo + 1);
        self.tokens.push(Token {
            lo: lo as u32,
            hi: lo as u32 + 1,
            data: group,
            kind,
            flags,
            aux: 0,
            _reserved: 0,
        });
        Ok(lo + 1)
    }

    // ---- Atoms. ----

    /// The atom of `text[lo..hi]`, as spelled: an index into `atoms`, made on first sight.
    #[inline(always)]
    fn atom(&mut self, lo: usize, hi: usize) -> u32 {
        match self.table.find(self.src, lo, hi - lo) {
            Probe::Hit(atom) => atom,
            Probe::Miss { hash } => self.new_atom(hash, lo, hi, 0),
        }
    }

    /// [`Scanner::atom`] for an identifier or lifetime that is not ASCII, whose symbol is its
    /// spelling normalized to NFC.
    fn atom_nfc(&mut self, lo: usize, hi: usize) -> u32 {
        match self.table.find(self.src, lo, hi - lo) {
            Probe::Hit(atom) => atom,
            Probe::Miss { hash } => self.new_atom(hash, lo, hi, ATOM_NFC),
        }
    }

    #[inline(always)]
    fn new_atom(&mut self, hash: u64, lo: usize, hi: usize, mut flags: u8) -> u32 {
        let atom = self.atoms.len() as u32;
        match &self.src[lo..hi] {
            b"macro_rules" => flags |= ATOM_MACRO_RULES,
            b"macro" => flags |= ATOM_MACRO,
            _ => {}
        }
        self.atoms.push(Atom { lo: lo as u32, len: (hi - lo) as u32, hash, flags });
        self.table.insert(hash, lo, hi - lo, atom);
        atom
    }

    /// Notes an identifier token as a gallery candidate if its atom has not been seen as an
    /// identifier before. Candidates are filtered by symbol once atoms are resolved.
    #[inline(always)]
    fn record_gallery(&mut self, atom: u32, token: u32) {
        let entry = &mut self.atoms[atom as usize];
        if entry.flags & ATOM_IDENT_SEEN == 0 {
            entry.flags |= ATOM_IDENT_SEEN;
            self.gallery_candidates.push((atom, token));
        }
    }

    // ---- Whitespace and comments. ----

    #[inline(always)]
    fn whitespace(&mut self, mut pos: usize) -> usize {
        // Bounded by the chunk: the bytes past its end are the next chunk's.
        let src = &self.src[..self.end];
        while let Some(&b) = src.get(pos) {
            match b {
                b'\n' => {
                    self.newlines.push(pos as u32);
                    pos += 1;
                }
                b' ' | b'\t' | 0x0B | 0x0C | b'\r' => pos += 1,
                _ => break,
            }
        }
        pos
    }

    /// Records the newlines and multi-byte characters of `text[lo..hi]`, which a token or
    /// comment spans, and stops at a bidi control. Returns whether it holds a `\r`.
    fn segment(&mut self, lo: usize, hi: usize) -> Result<bool, Stop> {
        let bytes = &self.src[lo..hi];
        for nl in memchr::memchr_iter(b'\n', bytes) {
            self.newlines.push((lo + nl) as u32);
        }
        if !bytes.is_ascii() {
            for (at, c) in self.text[lo..hi].char_indices() {
                let width = c.len_utf8();
                if width > 1 {
                    self.multibyte.push(((lo + at) as u32, width as u8));
                    if is_text_flow(c) {
                        return self.fail(lo + at, ErrorKind::TextFlow);
                    }
                }
            }
        }
        Ok(memchr::memchr(b'\r', bytes).is_some())
    }

    fn line_comment(&mut self, lo: usize) -> Step {
        let src = self.src;
        let hi = memchr::memchr(b'\n', &src[lo + 2..]).map_or(src.len(), |at| lo + 2 + at);
        let has_cr = self.segment(lo, hi)?;
        let doc = match src.get(lo + 2) {
            Some(b'!') => Some(1),
            Some(b'/') if src.get(lo + 3) != Some(&b'/') => Some(0),
            _ => None,
        };
        if let Some(inner) = doc {
            if has_cr && memchr::memchr(b'\r', &src[lo + 3..hi]).is_some() {
                return self.fail(lo, ErrorKind::CrInDocComment);
            }
            let atom = self.atom(lo + 3, hi);
            self.leaf(Kind::LineDoc, lo, hi, atom, 0, inner);
        }
        Ok(hi)
    }

    fn block_comment(&mut self, lo: usize) -> Step {
        let src = self.src;
        let mut depth = 1u32;
        let mut pos = lo + 2;
        loop {
            let Some(at) = memchr::memchr2(b'/', b'*', &src[pos..]) else {
                return self.fail(lo, ErrorKind::Unterminated);
            };
            pos += at;
            match (src[pos], src.get(pos + 1)) {
                (b'*', Some(b'/')) => {
                    depth -= 1;
                    pos += 2;
                    if depth == 0 {
                        break;
                    }
                }
                (b'/', Some(b'*')) => {
                    depth += 1;
                    pos += 2;
                }
                _ => pos += 1,
            }
        }
        let hi = pos;
        let has_cr = self.segment(lo, hi)?;
        let doc = match &src[lo + 2..] {
            [b'!', ..] => Some(1),
            [b'*', b'*' | b'/', ..] => None,
            [b'*', ..] => Some(0),
            _ => None,
        };
        if let Some(inner) = doc {
            if has_cr && memchr::memchr(b'\r', &src[lo + 3..hi - 2]).is_some() {
                return self.fail(lo, ErrorKind::CrInDocComment);
            }
            let atom = self.atom(lo + 3, hi - 2);
            self.leaf(Kind::BlockDoc, lo, hi, atom, 0, inner);
        }
        Ok(hi)
    }

    // ---- Identifiers. ----

    /// The end of the identifier continuation from `pos`, and whether it was all ASCII.
    #[inline(always)]
    fn ident_tail(&self, mut pos: usize) -> (usize, bool) {
        let src = self.src;
        while let Some(&b) = src.get(pos) {
            if CLASS[b as usize] & ID_CONT == 0 {
                if b >= 0x80 {
                    return (self.unicode_ident_tail(pos), false);
                }
                break;
            }
            pos += 1;
        }
        (pos, true)
    }

    #[cold]
    fn unicode_ident_tail(&self, mut pos: usize) -> usize {
        for c in self.text[pos..].chars() {
            if !is_id_continue(c) {
                break;
            }
            pos += c.len_utf8();
        }
        pos
    }

    /// An identifier from `lo`, whose first character is already known to start one.
    #[inline(always)]
    fn ident(&mut self, lo: usize) -> Step {
        let (hi, ascii) = self.ident_tail(lo + 1);
        self.ident_end(lo, hi, ascii)
    }

    #[inline(always)]
    fn ident_end(&mut self, lo: usize, hi: usize, ascii: bool) -> Step {
        match self.src.get(hi) {
            // An unknown prefix (reserved since 2021, linted before).
            Some(b'#' | b'"' | b'\'') => return self.fail(lo, ErrorKind::Reserved),
            Some(&b) if b >= 0x80 && self.emoji_at(hi) => {
                return self.fail(lo, ErrorKind::UnknownChar);
            }
            _ => {}
        }
        let atom = if ascii {
            self.atom(lo, hi)
        } else {
            self.segment(lo, hi)?;
            self.atom_nfc(lo, hi)
        };
        let index = self.leaf(Kind::Ident, lo, hi, atom, 0, 0);
        self.record_gallery(atom, index);
        Ok(hi)
    }

    #[cold]
    fn emoji_at(&self, pos: usize) -> bool {
        self.text[pos..].chars().next().is_some_and(|c| c.is_emoji_char())
    }

    #[cold]
    fn non_ascii(&mut self, lo: usize) -> Step {
        let c = self.text[lo..].chars().next().expect("a byte at lo");
        let hi = lo + c.len_utf8();
        if is_whitespace(c) {
            self.multibyte.push((lo as u32, c.len_utf8() as u8));
            return Ok(hi);
        }
        if is_id_start(c) {
            self.multibyte.push((lo as u32, c.len_utf8() as u8));
            let (end, _) = self.ident_tail(hi);
            // `ident_end` records the rest of the multi-byte characters; the first is recorded.
            return self.unicode_ident_end(lo, hi, end);
        }
        self.fail(lo, ErrorKind::UnknownChar)
    }

    /// `ident_end` for an identifier whose first character (`lo..first_end`) is not ASCII and
    /// already recorded in the multi-byte table.
    fn unicode_ident_end(&mut self, lo: usize, first_end: usize, hi: usize) -> Step {
        match self.src.get(hi) {
            Some(b'#' | b'"' | b'\'') => return self.fail(lo, ErrorKind::Reserved),
            Some(&b) if b >= 0x80 && self.emoji_at(hi) => {
                return self.fail(lo, ErrorKind::UnknownChar);
            }
            _ => {}
        }
        self.segment(first_end, hi)?;
        let atom = self.atom_nfc(lo, hi);
        let index = self.leaf(Kind::Ident, lo, hi, atom, 0, 0);
        self.record_gallery(atom, index);
        Ok(hi)
    }

    /// Whether an identifier can start at `pos`.
    #[inline]
    fn id_start_at(&self, pos: usize) -> bool {
        match self.src.get(pos) {
            Some(&b) if b < 0x80 => CLASS[b as usize] & ID_START != 0,
            Some(_) => self.text[pos..].chars().next().is_some_and(is_id_start),
            None => false,
        }
    }

    /// `r`: a raw identifier, a raw string, or an identifier.
    fn r_prefixed(&mut self, lo: usize) -> Step {
        match self.src.get(lo + 1) {
            Some(b'#') if self.id_start_at(lo + 2) => self.raw_ident(lo),
            Some(b'#' | b'"') => self.raw_string(lo, 1, Kind::StrRaw, Mode::RawStr),
            _ => self.ident(lo),
        }
    }

    fn raw_ident(&mut self, lo: usize) -> Step {
        let name = lo + 2;
        let (hi, ascii) = if self.src[name] < 0x80 {
            self.ident_tail(name + 1)
        } else {
            let first = self.text[name..].chars().next().expect("an identifier start");
            let (hi, _) = self.ident_tail(name + first.len_utf8());
            (hi, false)
        };
        let spelling = &self.text[name..hi];
        if matches!(spelling, "_" | "super" | "self" | "Self" | "crate") {
            return self.fail(lo, ErrorKind::Reserved);
        }
        let atom = if ascii {
            self.atom(name, hi)
        } else {
            self.segment(name, hi)?;
            self.atom_nfc(name, hi)
        };
        let index = self.leaf(Kind::Ident, lo, hi, atom, FLAG_RAW, 1);
        self.raw_idents.push(index);
        self.record_gallery(atom, index);
        Ok(hi)
    }

    /// `b`: a byte, a byte string, a raw byte string, or an identifier.
    fn b_prefixed(&mut self, lo: usize) -> Step {
        let src = self.src;
        match (src.get(lo + 1), src.get(lo + 2)) {
            (Some(b'\''), _) => self.char_literal(lo, lo + 2, Kind::Byte, Mode::Byte),
            (Some(b'"'), _) => self.string(lo, lo + 2, Kind::ByteStr, Mode::ByteStr),
            (Some(b'r'), Some(b'"' | b'#')) => {
                self.raw_string(lo, 2, Kind::ByteStrRaw, Mode::RawByteStr)
            }
            _ => self.ident(lo),
        }
    }

    /// `c`: a C string, a raw C string, or an identifier.
    fn c_prefixed(&mut self, lo: usize) -> Step {
        let src = self.src;
        let literal = match (src.get(lo + 1), src.get(lo + 2)) {
            (Some(b'"'), _) => Some(false),
            (Some(b'r'), Some(b'"' | b'#')) => Some(true),
            _ => None,
        };
        match literal {
            // C strings are literals from 2021; before, the parser reports them.
            Some(_) if self.edition < Edition::Edition2021 => self.fail(lo, ErrorKind::Reserved),
            Some(false) => self.string(lo, lo + 2, Kind::CStr, Mode::CStr),
            Some(true) => self.raw_string(lo, 2, Kind::CStrRaw, Mode::RawCStr),
            None => self.ident(lo),
        }
    }

    // ---- Literals. ----

    /// A literal suffix from `pos`: its end, and its span if there is one.
    #[inline(always)]
    fn suffix(&mut self, pos: usize) -> Result<(usize, Option<(usize, usize)>), Stop> {
        if !self.id_start_at(pos) {
            return Ok((pos, None));
        }
        let (hi, ascii) = if self.src[pos] < 0x80 {
            self.ident_tail(pos + 1)
        } else {
            let first = self.text[pos..].chars().next().expect("an identifier start");
            (self.ident_tail(pos + first.len_utf8()).0, false)
        };
        if !ascii {
            self.segment(pos, hi)?;
        }
        if hi - pos == 1 && self.src[pos] == b'_' {
            return self.fail(pos, ErrorKind::InvalidLiteral);
        }
        Ok((hi, Some((pos, hi))))
    }

    /// Pushes a literal token whose text is `lo..hi`, contents `content`, suffix `suffix`.
    #[inline(always)]
    fn literal(
        &mut self,
        kind: Kind,
        lo: usize,
        hi: usize,
        content: (usize, usize),
        suffix: Option<(usize, usize)>,
        aux: u8,
    ) {
        let atom = self.atom(content.0, content.1);
        let flags = if suffix.is_some() { FLAG_SUFFIX } else { 0 };
        let index = self.leaf(kind, lo, hi, atom, flags, aux);
        if let Some((slo, shi)) = suffix {
            let suffix = self.atom(slo, shi);
            self.suffixes.push((index, suffix));
        }
    }

    /// Digits and `_` from `pos`, decimal or hexadecimal: the end, and whether a digit occurred.
    #[inline(always)]
    fn digits(&self, mut pos: usize, hex: bool) -> (usize, bool) {
        let src = self.src;
        let mut any = false;
        while let Some(&b) = src.get(pos) {
            if b.is_ascii_digit() || (hex && b.is_ascii_hexdigit()) {
                any = true;
            } else if b != b'_' {
                break;
            }
            pos += 1;
        }
        (pos, any)
    }

    fn number(&mut self, lo: usize) -> Step {
        let src = self.src;
        let mut pos = lo + 1;
        let mut base = 10u32;
        let mut int_only = false;
        if src[lo] == b'0' {
            match src.get(pos) {
                Some(b'b') | Some(b'o') => {
                    base = if src[pos] == b'b' { 2 } else { 8 };
                    let (end, any) = self.digits(pos + 1, false);
                    if !any {
                        return self.fail(lo, ErrorKind::InvalidLiteral);
                    }
                    let limit = if base == 2 { b'1' } else { b'7' };
                    if src[pos + 1..end].iter().any(|&d| d != b'_' && d > limit) {
                        return self.fail(lo, ErrorKind::InvalidLiteral);
                    }
                    pos = end;
                }
                Some(b'x') => {
                    base = 16;
                    let (end, any) = self.digits(pos + 1, true);
                    if !any {
                        return self.fail(lo, ErrorKind::InvalidLiteral);
                    }
                    pos = end;
                }
                Some(b'0'..=b'9' | b'_') => pos = self.digits(pos, false).0,
                Some(b'.' | b'e' | b'E') => {}
                _ => int_only = true,
            }
        } else {
            pos = self.digits(pos, false).0;
        }
        let mut float = false;
        if !int_only {
            match src.get(pos) {
                Some(b'.') if src.get(pos + 1) != Some(&b'.') && !self.id_start_at(pos + 1) => {
                    float = true;
                    pos += 1;
                    if src.get(pos).is_some_and(u8::is_ascii_digit) {
                        pos = self.digits(pos, false).0;
                        if matches!(src.get(pos), Some(b'e' | b'E')) {
                            pos = self.exponent(lo, pos + 1)?;
                        }
                    }
                }
                Some(b'e' | b'E') => {
                    float = true;
                    pos = self.exponent(lo, pos + 1)?;
                }
                _ => {}
            }
        }
        if float && base != 10 {
            return self.fail(lo, ErrorKind::InvalidLiteral);
        }
        let digits_end = pos;
        let (hi, suffix) = self.suffix(pos)?;
        let kind = if float { Kind::Float } else { Kind::Integer };
        self.literal(kind, lo, hi, (lo, digits_end), suffix, 0);
        Ok(hi)
    }

    /// After `e`: an optional sign and at least one digit.
    fn exponent(&mut self, lo: usize, mut pos: usize) -> Step {
        if matches!(self.src.get(pos), Some(b'+' | b'-')) {
            pos += 1;
        }
        let (end, any) = self.digits(pos, false);
        if !any {
            return self.fail(lo, ErrorKind::InvalidLiteral);
        }
        Ok(end)
    }

    /// A double-quoted string from `lo`, its contents starting at `content_lo` (after the
    /// prefix and the quote).
    fn string(&mut self, lo: usize, content_lo: usize, kind: Kind, mode: Mode) -> Step {
        let src = self.src;
        let mut pos = content_lo;
        let mut escapes = false;
        loop {
            let Some(at) = memchr::memchr2(b'"', b'\\', &src[pos..]) else {
                return self.fail(lo, ErrorKind::Unterminated);
            };
            pos += at;
            if src[pos] == b'"' {
                break;
            }
            escapes = true;
            // The escaped character, whole: a quote or backslash, or the first byte of any
            // other (a continuation byte is never `"` or `\`).
            pos += 2;
            if pos > src.len() {
                return self.fail(lo, ErrorKind::Unterminated);
            }
        }
        let content_hi = pos;
        let has_cr = self.segment(content_lo, content_hi)?;
        let check = match mode {
            Mode::Str => escapes || has_cr,
            _ => true,
        };
        if check && !self.escapes_ok(content_lo, content_hi, mode) {
            return self.fail(lo, ErrorKind::InvalidLiteral);
        }
        let (hi, suffix) = self.suffix(content_hi + 1)?;
        self.literal(kind, lo, hi, (content_lo, content_hi), suffix, 0);
        Ok(hi)
    }

    fn escapes_ok(&self, lo: usize, hi: usize, mode: Mode) -> bool {
        // Called once per escape problem, fatal or a warning; either needs a diagnostic.
        let mut clean = true;
        check_for_errors(&self.text[lo..hi], mode, |_, _| clean = false);
        clean
    }

    /// A raw string from `lo`: `prefix` bytes (`r`, `br` or `cr`), then `#`s and a quote.
    fn raw_string(&mut self, lo: usize, prefix: usize, kind: Kind, mode: Mode) -> Step {
        let src = self.src;
        let mut pos = lo + prefix;
        let hashes_lo = pos;
        while src.get(pos) == Some(&b'#') {
            pos += 1;
        }
        let hashes = pos - hashes_lo;
        if src.get(pos) != Some(&b'"') || hashes > 255 {
            return self.fail(lo, ErrorKind::InvalidLiteral);
        }
        let content_lo = pos + 1;
        pos = content_lo;
        let content_hi = loop {
            let Some(at) = memchr::memchr(b'"', &src[pos..]) else {
                return self.fail(lo, ErrorKind::Unterminated);
            };
            pos += at;
            let quote = pos;
            pos += 1;
            let closing = src[pos..].iter().take(hashes).take_while(|&&b| b == b'#').count();
            if closing == hashes {
                pos += hashes;
                break quote;
            }
        };
        let has_cr = self.segment(content_lo, content_hi)?;
        let check = match mode {
            Mode::RawStr => has_cr,
            _ => true,
        };
        if check && !self.escapes_ok(content_lo, content_hi, mode) {
            return self.fail(lo, ErrorKind::InvalidLiteral);
        }
        let (hi, suffix) = self.suffix(pos)?;
        self.literal(kind, lo, hi, (content_lo, content_hi), suffix, hashes as u8);
        Ok(hi)
    }

    /// A character literal (or byte literal) from `lo`, its contents starting at `content_lo`
    /// (after the prefix and the quote).
    fn char_literal(&mut self, lo: usize, content_lo: usize, kind: Kind, mode: Mode) -> Step {
        let text = self.text;
        let mut chars = text[content_lo..].chars();
        let (first, second) = (chars.next(), chars.next());
        let mut pos = content_lo;
        let terminated = if second == Some('\'') && first != Some('\\') {
            pos += first.map_or(0, char::len_utf8) + 1;
            true
        } else {
            let mut rest = text[content_lo..].chars().peekable();
            loop {
                match rest.next() {
                    Some('\'') => {
                        pos += 1;
                        break true;
                    }
                    Some('/') | None => break false,
                    Some('\n') if rest.peek() != Some(&'\'') => break false,
                    Some('\\') => {
                        pos += 1;
                        if let Some(c) = rest.next() {
                            pos += c.len_utf8();
                        }
                    }
                    Some(c) => pos += c.len_utf8(),
                }
            }
        };
        if !terminated {
            return self.fail(lo, ErrorKind::Unterminated);
        }
        let content_hi = pos - 1;
        self.segment(content_lo, content_hi)?;
        if !self.escapes_ok(content_lo, content_hi, mode) {
            return self.fail(lo, ErrorKind::InvalidLiteral);
        }
        let (hi, suffix) = self.suffix(pos)?;
        self.literal(kind, lo, hi, (content_lo, content_hi), suffix, 0);
        Ok(hi)
    }

    /// `'`: a lifetime or a character literal.
    fn lifetime_or_char(&mut self, lo: usize) -> Step {
        let text = self.text;
        let mut chars = text[lo + 1..].chars();
        let first = chars.next();
        let second = chars.next();
        let can_be_lifetime =
            second != Some('\'') && first.is_some_and(|c| is_id_start(c) || c.is_ascii_digit());
        if !can_be_lifetime {
            return self.char_literal(lo, lo + 1, Kind::Char, Mode::Char);
        }
        let first = first.expect("checked above");
        if first == 'r' && second == Some('#') && chars.next().is_some_and(is_id_start) {
            // A raw lifetime.
            return self.fail(lo, ErrorKind::Reserved);
        }
        let starts_with_number = first.is_ascii_digit();
        let (hi, ascii) = self.ident_tail(lo + 1 + first.len_utf8());
        match self.src.get(hi) {
            // `'ab'`: a character literal of more than one character, which is an error.
            Some(b'\'') => self.fail(lo, ErrorKind::InvalidLiteral),
            Some(b'#') if !starts_with_number => self.fail(lo, ErrorKind::Reserved),
            _ if starts_with_number => self.fail(lo, ErrorKind::Reserved),
            _ => {
                let atom = if ascii && first.is_ascii() {
                    self.atom(lo, hi)
                } else {
                    self.segment(lo, hi)?;
                    self.atom_nfc(lo, hi)
                };
                self.leaf(Kind::Lifetime, lo, hi, atom, 0, 0);
                Ok(hi)
            }
        }
    }

    // ---- Punctuation. ----

    /// The longest punctuation token at `lo`, glued as the parser would glue joint pieces.
    #[inline(always)]
    fn punct(&mut self, lo: usize) -> usize {
        use Kind::*;
        let src = self.src;
        let b1 = src.get(lo + 1).copied().unwrap_or(0);
        let b2 = src.get(lo + 2).copied().unwrap_or(0);
        let (kind, len) = match (src[lo], b1, b2) {
            (b'.', b'.', b'=') => (DotDotEq, 3),
            (b'.', b'.', b'.') => (DotDotDot, 3),
            (b'<', b'<', b'=') => (ShlEq, 3),
            (b'>', b'>', b'=') => (ShrEq, 3),
            (b'<', b'-', _) => (LArrow, 2),
            (b'-', b'>', _) => (RArrow, 2),
            (b'=', b'>', _) => (FatArrow, 2),
            (b':', b':', _) => (PathSep, 2),
            (b'.', b'.', _) => (DotDot, 2),
            (b'=', b'=', _) => (EqEq, 2),
            (b'!', b'=', _) => (Ne, 2),
            (b'<', b'=', _) => (Le, 2),
            (b'>', b'=', _) => (Ge, 2),
            (b'&', b'&', _) => (AndAnd, 2),
            (b'|', b'|', _) => (OrOr, 2),
            (b'<', b'<', _) => (Shl, 2),
            (b'>', b'>', _) => (Shr, 2),
            (b'+', b'=', _) => (PlusEq, 2),
            (b'-', b'=', _) => (MinusEq, 2),
            (b'*', b'=', _) => (StarEq, 2),
            (b'/', b'=', _) => (SlashEq, 2),
            (b'%', b'=', _) => (PercentEq, 2),
            (b'^', b'=', _) => (CaretEq, 2),
            (b'&', b'=', _) => (AndEq, 2),
            (b'|', b'=', _) => (OrEq, 2),
            (b'=', ..) => (Eq, 1),
            (b'<', ..) => (Lt, 1),
            (b'>', ..) => (Gt, 1),
            (b'!', ..) => (Bang, 1),
            (b'~', ..) => (Tilde, 1),
            (b'+', ..) => (Plus, 1),
            (b'-', ..) => (Minus, 1),
            (b'*', ..) => (Star, 1),
            (b'/', ..) => (Slash, 1),
            (b'%', ..) => (Percent, 1),
            (b'^', ..) => (Caret, 1),
            (b'&', ..) => (And, 1),
            (b'|', ..) => (Or, 1),
            (b'@', ..) => (At, 1),
            (b'.', ..) => (Dot, 1),
            (b',', ..) => (Comma, 1),
            (b';', ..) => (Semi, 1),
            (b':', ..) => (Colon, 1),
            (b'#', ..) => (Pound, 1),
            (b'$', ..) => (Dollar, 1),
            (b'?', ..) => (Question, 1),
            _ => unreachable!("punct is only called on a punctuation byte"),
        };
        self.leaf(kind, lo, lo + len, 0, 0, 0);
        lo + len
    }

    // ---- The start of a file. ----

    /// A frontmatter fence (`---`) opening the file at `pos`, after whitespace.
    fn frontmatter_at(&self, pos: usize) -> bool {
        let rest = &self.src[pos..];
        let skipped = rest.iter().take_while(|&&b| is_ascii_ws(b)).count();
        rest[skipped..].starts_with(b"---")
    }
}

/// The length of a shebang line opening `input`, as `rustc_lexer::strip_shebang` finds it: `#!`
/// and the rest of the line, unless the next token after whitespace and plain comments is `[`.
pub(crate) fn shebang_len(input: &str) -> Option<usize> {
    let tail = input.strip_prefix("#!")?;
    let bytes = tail.as_bytes();
    let mut pos = 0;
    loop {
        match bytes.get(pos) {
            Some(&b) if is_ascii_ws(b) => pos += 1,
            Some(b'/') => match comment_at(&bytes[pos..]) {
                Some(false) if bytes[pos + 1] == b'/' => {
                    pos = memchr::memchr(b'\n', &bytes[pos..]).map_or(bytes.len(), |at| pos + at);
                }
                Some(false) => {
                    let mut depth = 1u32;
                    let mut at = pos + 2;
                    while depth > 0 {
                        match (bytes.get(at), bytes.get(at + 1)) {
                            (None, _) => break,
                            (Some(b'*'), Some(b'/')) => {
                                depth -= 1;
                                at += 2;
                            }
                            (Some(b'/'), Some(b'*')) => {
                                depth += 1;
                                at += 2;
                            }
                            _ => at += 1,
                        }
                    }
                    pos = at;
                }
                _ => break,
            },
            Some(b'[') => return None,
            _ => break,
        }
    }
    Some(2 + tail.lines().next().unwrap_or_default().len())
}

#[allow(dead_code)]
fn _assert_send_sync()
where
    LexedFile: Send + Sync,
    Box<[Token]>: Send + Sync,
{
}

/// The file's error, if a chunk has one: the first, since chunks come in order.
pub(crate) fn first_error(chunks: &[Chunk]) -> Option<LexError> {
    chunks.iter().find_map(|chunk| chunk.error)
}

/// The empty file that carries a lexing error.
pub(crate) fn failed(src: Arc<String>, begin: usize, error: LexError) -> LexedFile {
    LexedFile {
        src,
        begin: begin as u32,
        tokens: Box::new([]),
        groups: Box::new([]),
        item_cuts: Box::new([]),
        raw_idents: Box::new([]),
        suffixes: Box::new([]),
        newlines: Box::new([]),
        multibyte: Box::new([]),
        gallery: Box::new([]),
        error: Some(error),
    }
}

/// No symbol found for an atom (yet): see [`Chunk::look_up`].
const UNKNOWN: u32 = u32::MAX;

impl Chunk {
    /// Finds the symbol of each of this chunk's atoms that `interner` already has, without
    /// adding any: the part of resolving that many chunks can do at once, each on its own task.
    /// [`resolve`] then interns only the rest, in order.
    pub(crate) fn look_up(&mut self, text: &str, interner: &Interner) {
        self.known = self
            .atoms
            .iter()
            .map(|atom| {
                if atom.flags & ATOM_NFC != 0 {
                    return UNKNOWN;
                }
                let spelling = &text[atom.lo as usize..(atom.lo + atom.len) as usize];
                interner.find_hashed(spelling, atom.hash).map_or(UNKNOWN, Symbol::as_u32)
            })
            .collect();
    }
}

/// Each chunk's atoms as symbols: atoms are deduplicated across chunks in order of first
/// occurrence, and each distinct spelling is interned once (the one place lexing adds to the
/// shared interner, in an order that does not depend on how chunks were scheduled). Atoms a
/// chunk already found ([`Chunk::look_up`]) are taken as found; the work here is in proportion
/// to the spellings the interner did not have.
pub(crate) fn resolve(
    text: &Arc<String>,
    chunks: &mut [Chunk],
    interner: &Interner,
) -> Vec<Vec<u32>> {
    let bytes = text.as_bytes();
    let intern = |atom: &Atom| {
        let (lo, hi) = (atom.lo as usize, (atom.lo + atom.len) as usize);
        let spelling = &text[lo..hi];
        let symbol =
            if atom.flags & ATOM_NFC != 0 && is_nfc_quick(spelling.chars()) != IsNormalized::Yes {
                let normalized: String = spelling.chars().nfc().collect();
                interner.intern(&normalized)
            } else {
                interner.intern_hashed_from_source(spelling, atom.hash, text)
            };
        symbol.as_u32()
    };
    if let [chunk] = chunks {
        return alloc::vec![chunk.atoms.iter().map(intern).collect()];
    }
    // The table holds each distinct spelling not yet found once: size it by those.
    let unknown: usize = chunks
        .iter()
        .map(|chunk| {
            if chunk.known.is_empty() {
                chunk.atoms.len()
            } else {
                chunk.known.iter().filter(|&&s| s == UNKNOWN).count()
            }
        })
        .sum();
    let mut table = Table::with_capacity(unknown);
    let mut symbols: Vec<u32> = Vec::with_capacity(unknown);
    chunks
        .iter_mut()
        .map(|chunk| {
            let mut out = core::mem::take(&mut chunk.known);
            if out.is_empty() {
                out = alloc::vec![UNKNOWN; chunk.atoms.len()];
            }
            for (atom, symbol) in chunk.atoms.iter().zip(&mut out) {
                if *symbol != UNKNOWN {
                    continue;
                }
                let (lo, len) = (atom.lo as usize, atom.len as usize);
                *symbol = match table.find_hashed(bytes, lo, len, atom.hash) {
                    Some(id) => symbols[id as usize],
                    None => {
                        let new = intern(atom);
                        table.insert(atom.hash, lo, len, symbols.len() as u32);
                        symbols.push(new);
                        new
                    }
                };
            }
            out
        })
        .collect()
}

/// Rewrites one chunk to file numbering: identifier and literal payloads, suffixes and gallery
/// candidates from atoms to `symbols`; token numbers offset by `token_base`, group numbers by
/// `group_base`. Pure and owned: one task per chunk.
pub(crate) fn patch(chunk: &mut Chunk, symbols: &[u32], token_base: u32, group_base: u32) {
    for token in &mut chunk.tokens {
        if token.kind >= Kind::Ident {
            if token.kind != Kind::Eof {
                token.data = symbols[token.data as usize];
            }
        } else if token.kind.is_open_delim() || token.kind.is_close_delim() {
            token.data += group_base;
        }
    }
    if token_base != 0 || group_base != 0 {
        for g in &mut chunk.groups {
            g.open += token_base;
            g.close += token_base;
            if g.parent != NO_GROUP {
                g.parent += group_base;
            }
        }
        for t in &mut chunk.raw_idents {
            *t += token_base;
        }
    }
    for (token, atom) in &mut chunk.suffixes {
        *token += token_base;
        *atom = symbols[*atom as usize];
    }
    for (atom, token) in &mut chunk.gallery_candidates {
        *atom = symbols[*atom as usize];
        *token += token_base;
    }
}

/// A file of one patched chunk: its buffers are moved, not copied.
pub(crate) fn single(src: Arc<String>, begin: usize, chunk: Chunk) -> LexedFile {
    LexedFile {
        src,
        begin: begin as u32,
        gallery: gallery(chunk.gallery_candidates.iter().copied()),
        suffixes: chunk
            .suffixes
            .iter()
            .map(|&(token, symbol)| (token, Symbol::new(symbol)))
            .collect(),
        tokens: chunk.tokens.into_boxed_slice(),
        groups: chunk.groups.into_boxed_slice(),
        item_cuts: chunk.item_cuts.into_boxed_slice(),
        raw_idents: chunk.raw_idents.into_boxed_slice(),
        newlines: chunk.newlines.into_boxed_slice(),
        multibyte: chunk.multibyte.into_boxed_slice(),
        error: None,
    }
}

/// The file from buffers its parts filled.
pub(crate) fn from_filled(src: Arc<String>, begin: usize, filled: Filled) -> LexedFile {
    LexedFile {
        src,
        begin: begin as u32,
        gallery: gallery(filled.gallery.iter().copied()),
        tokens: filled.tokens,
        groups: filled.groups,
        item_cuts: filled.item_cuts,
        raw_idents: filled.raw_idents,
        suffixes: filled.suffixes,
        newlines: filled.newlines,
        multibyte: filled.multibyte,
        error: None,
    }
}

/// Each symbol's first identifier occurrence, from patched candidates in token order;
/// spellings that normalize alike share one.
fn gallery(candidates: impl Iterator<Item = (u32, u32)>) -> Box<[(Symbol, u32)]> {
    let mut gallery = Vec::new();
    let mut seen: Vec<u64> = Vec::new();
    for (symbol, token) in candidates {
        let (word, bit) = (symbol as usize / 64, 1u64 << (symbol % 64));
        if word >= seen.len() {
            seen.resize(word + 1, 0);
        }
        if seen[word] & bit == 0 {
            seen[word] |= bit;
            gallery.push((Symbol::new(symbol), token));
        }
    }
    gallery.into_boxed_slice()
}
