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

use frontend_symbol::{Edition, Interner, Symbol, kw};
use rustc_literal_escaper::{Mode, check_for_errors};
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};
use unicode_properties::UnicodeEmoji;

use crate::table::{Probe, Table};
use crate::{
    Delimiter, ErrorKind, FLAG_JOINT, FLAG_JOINT_HIDDEN, FLAG_RAW, FLAG_SUFFIX, Group, Kind,
    LexError, LexedFile, NO_GROUP, Options, Token,
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

/// Stop the pass: the input needs a diagnostic at a position already recorded.
struct Stop;

type Step = Result<usize, Stop>;

pub(crate) struct Scanner<'a> {
    text: &'a str,
    src: &'a [u8],
    owner: &'a Arc<String>,
    interner: &'a Interner,
    edition: Edition,
    table: Table,
    tokens: Vec<Token>,
    groups: Vec<Group>,
    frames: Vec<Frame>,
    item_cuts: Vec<u32>,
    raw_idents: Vec<u32>,
    suffixes: Vec<(u32, Symbol)>,
    newlines: Vec<u32>,
    multibyte: Vec<(u32, u8)>,
    gallery: Vec<(Symbol, u32)>,
    gallery_seen: Vec<u64>,
    error: Option<LexError>,
}

impl<'a> Scanner<'a> {
    pub(crate) fn run(src: Arc<String>, options: Options, interner: &Interner) -> LexedFile {
        let begin = if options.strip_shebang { shebang_len(&src).unwrap_or(0) } else { 0 };
        let (tokens, groups, item_cuts, raw_idents, suffixes, newlines, multibyte, gallery, error) = {
            let text = src.as_str();
            let len = text.len();
            let mut s = Scanner {
                text,
                src: text.as_bytes(),
                owner: &src,
                interner,
                edition: options.edition,
                table: Table::for_source(len),
                tokens: Vec::with_capacity(len / 4 + 16),
                groups: Vec::with_capacity(len / 32 + 8),
                frames: alloc::vec![Frame::new(NO_GROUP, false)],
                item_cuts: Vec::new(),
                raw_idents: Vec::new(),
                suffixes: Vec::new(),
                newlines: Vec::with_capacity(len / 32 + 8),
                multibyte: Vec::new(),
                gallery: Vec::new(),
                gallery_seen: Vec::new(),
                error: None,
            };
            if options.frontmatter && s.frontmatter_at(begin) {
                let _ = s.fail::<()>(begin, ErrorKind::Frontmatter);
            } else {
                let _ = s.lex(begin);
            }
            (
                s.tokens.into_boxed_slice(),
                s.groups.into_boxed_slice(),
                s.item_cuts.into_boxed_slice(),
                s.raw_idents.into_boxed_slice(),
                s.suffixes.into_boxed_slice(),
                s.newlines.into_boxed_slice(),
                s.multibyte.into_boxed_slice(),
                s.gallery.into_boxed_slice(),
                s.error,
            )
        };
        LexedFile {
            src,
            begin: begin as u32,
            tokens,
            groups,
            item_cuts,
            raw_idents,
            suffixes,
            newlines,
            multibyte,
            gallery,
            error,
        }
    }

    // ---- The loop. ----

    fn lex(&mut self, mut pos: usize) -> Result<(), Stop> {
        let src = self.src;
        loop {
            let Some(&b) = src.get(pos) else {
                return self.finish(pos);
            };
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

    fn finish(&mut self, end: usize) -> Result<(), Stop> {
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
        let (macro_rules, decl_macro) = if kind == Kind::Ident {
            (data == kw::MacroRules.as_u32(), flags & FLAG_RAW == 0 && data == kw::Macro.as_u32())
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

    // ---- Symbols. ----

    /// The symbol of `text[lo..hi]`, as spelled.
    #[inline(always)]
    fn intern(&mut self, lo: usize, hi: usize) -> Symbol {
        match self.table.find(self.src, lo, hi - lo) {
            Probe::Hit(symbol) => symbol,
            Probe::Miss { hash } => {
                let symbol =
                    self.interner.intern_hashed_from_source(&self.text[lo..hi], hash, self.owner);
                self.table.insert(hash, lo, hi - lo, symbol);
                symbol
            }
        }
    }

    /// The symbol of `text[lo..hi]` normalized to NFC, for identifiers and lifetimes that are
    /// not ASCII. ASCII text is NFC and takes [`Scanner::intern`].
    fn intern_nfc(&mut self, lo: usize, hi: usize) -> Symbol {
        match self.table.find(self.src, lo, hi - lo) {
            Probe::Hit(symbol) => symbol,
            Probe::Miss { hash } => {
                let text = &self.text[lo..hi];
                let symbol = match is_nfc_quick(text.chars()) {
                    IsNormalized::Yes => {
                        self.interner.intern_hashed_from_source(text, hash, self.owner)
                    }
                    _ => {
                        let normalized: String = text.chars().nfc().collect();
                        self.interner.intern(&normalized)
                    }
                };
                self.table.insert(hash, lo, hi - lo, symbol);
                symbol
            }
        }
    }

    /// Records an identifier token's symbol in the gallery the first time the file uses it.
    #[inline(always)]
    fn record_gallery(&mut self, symbol: Symbol, token: u32) {
        let index = symbol.as_u32() as usize;
        let (word, bit) = (index / 64, 1u64 << (index % 64));
        if word >= self.gallery_seen.len() {
            self.gallery_seen.resize(word + 1, 0);
        }
        if self.gallery_seen[word] & bit == 0 {
            self.gallery_seen[word] |= bit;
            self.gallery.push((symbol, token));
        }
    }

    // ---- Whitespace and comments. ----

    #[inline(always)]
    fn whitespace(&mut self, mut pos: usize) -> usize {
        let src = self.src;
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
            let symbol = self.intern(lo + 3, hi);
            self.leaf(Kind::LineDoc, lo, hi, symbol.as_u32(), 0, inner);
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
            let symbol = self.intern(lo + 3, hi - 2);
            self.leaf(Kind::BlockDoc, lo, hi, symbol.as_u32(), 0, inner);
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
        let symbol = if ascii {
            self.intern(lo, hi)
        } else {
            self.segment(lo, hi)?;
            self.intern_nfc(lo, hi)
        };
        let index = self.leaf(Kind::Ident, lo, hi, symbol.as_u32(), 0, 0);
        self.record_gallery(symbol, index);
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
        let symbol = self.intern_nfc(lo, hi);
        let index = self.leaf(Kind::Ident, lo, hi, symbol.as_u32(), 0, 0);
        self.record_gallery(symbol, index);
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
        let symbol = if ascii {
            self.intern(name, hi)
        } else {
            self.segment(name, hi)?;
            self.intern_nfc(name, hi)
        };
        let index = self.leaf(Kind::Ident, lo, hi, symbol.as_u32(), FLAG_RAW, 1);
        self.raw_idents.push(index);
        self.record_gallery(symbol, index);
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
        let symbol = self.intern(content.0, content.1);
        let flags = if suffix.is_some() { FLAG_SUFFIX } else { 0 };
        let index = self.leaf(kind, lo, hi, symbol.as_u32(), flags, aux);
        if let Some((slo, shi)) = suffix {
            let suffix = self.intern(slo, shi);
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
                let symbol = if ascii && first.is_ascii() {
                    self.intern(lo, hi)
                } else {
                    self.segment(lo, hi)?;
                    self.intern_nfc(lo, hi)
                };
                self.leaf(Kind::Lifetime, lo, hi, symbol.as_u32(), 0, 0);
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
