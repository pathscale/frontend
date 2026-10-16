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

mod scan;
mod table;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;

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

/// Lexes `src` in one forward pass, interning through `interner`.
pub fn lex(src: Arc<String>, options: Options, interner: &Interner) -> Arc<LexedFile> {
    Arc::new(scan::Scanner::run(src, options, interner))
}

/// The length of a shebang line at the start of `input`, if it has one: `#!` not followed
/// (after whitespace and comments) by `[`, which would make it an inner attribute.
pub fn shebang_len(input: &str) -> Option<usize> {
    scan::shebang_len(input)
}
