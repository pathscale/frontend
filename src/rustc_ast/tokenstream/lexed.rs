//! A `frontend_lex::LexedFile` read as parser tokens.
//!
//! The lexer has already interned every identifier, lifetime, literal and doc comment, so a
//! token here is built from the file's arrays alone: no source text is read and nothing is
//! interned. Spans are the token's offsets plus the file's position in the source map.

use frontend_lex::{Kind, Token as LexedToken};

use super::Spacing;
use crate::rustc_ast::ast::AttrStyle;
use crate::rustc_ast::token::{self, CommentKind, IdentIsRaw, Token};
use crate::rustc_span::{BytePos, DUMMY_SP, Span, Symbol};

#[inline]
pub(crate) fn spacing(spacing: frontend_lex::Spacing) -> Spacing {
    match spacing {
        frontend_lex::Spacing::Alone => Spacing::Alone,
        frontend_lex::Spacing::Joint => Spacing::Joint,
        frontend_lex::Spacing::JointHidden => Spacing::JointHidden,
    }
}

/// The span of `lo..hi` in a file that starts at `base`.
#[inline]
pub(crate) fn span(base: BytePos, lo: u32, hi: u32) -> Span {
    Span::with_root_ctxt(base + BytePos(lo), base + BytePos(hi))
}

/// Decode one borrowed lexer token. `index` is used only for the optional suffix lookup.
#[inline(always)]
pub(crate) fn token(
    t: &LexedToken,
    base: BytePos,
    suffixes: &[(u32, Symbol)],
    index: u32,
) -> (Token, Spacing) {
    let spacing = spacing(t.spacing());
    if t.kind == Kind::Eof {
        return (Token::new(token::Eof, DUMMY_SP), spacing);
    }
    let span = span(base, t.lo, t.hi);
    let kind = match t.kind {
        Kind::Eq => token::Eq,
        Kind::Lt => token::Lt,
        Kind::Le => token::Le,
        Kind::EqEq => token::EqEq,
        Kind::Ne => token::Ne,
        Kind::Ge => token::Ge,
        Kind::Gt => token::Gt,
        Kind::AndAnd => token::AndAnd,
        Kind::OrOr => token::OrOr,
        Kind::Bang => token::Bang,
        Kind::Tilde => token::Tilde,
        Kind::Plus => token::Plus,
        Kind::Minus => token::Minus,
        Kind::Star => token::Star,
        Kind::Slash => token::Slash,
        Kind::Percent => token::Percent,
        Kind::Caret => token::Caret,
        Kind::And => token::And,
        Kind::Or => token::Or,
        Kind::Shl => token::Shl,
        Kind::Shr => token::Shr,
        Kind::PlusEq => token::PlusEq,
        Kind::MinusEq => token::MinusEq,
        Kind::StarEq => token::StarEq,
        Kind::SlashEq => token::SlashEq,
        Kind::PercentEq => token::PercentEq,
        Kind::CaretEq => token::CaretEq,
        Kind::AndEq => token::AndEq,
        Kind::OrEq => token::OrEq,
        Kind::ShlEq => token::ShlEq,
        Kind::ShrEq => token::ShrEq,
        Kind::At => token::At,
        Kind::Dot => token::Dot,
        Kind::DotDot => token::DotDot,
        Kind::DotDotDot => token::DotDotDot,
        Kind::DotDotEq => token::DotDotEq,
        Kind::Comma => token::Comma,
        Kind::Semi => token::Semi,
        Kind::Colon => token::Colon,
        Kind::PathSep => token::PathSep,
        Kind::RArrow => token::RArrow,
        Kind::LArrow => token::LArrow,
        Kind::FatArrow => token::FatArrow,
        Kind::Pound => token::Pound,
        Kind::Dollar => token::Dollar,
        Kind::Question => token::Question,
        Kind::OpenParen => token::OpenParen,
        Kind::CloseParen => token::CloseParen,
        Kind::OpenBrace => token::OpenBrace,
        Kind::CloseBrace => token::CloseBrace,
        Kind::OpenBracket => token::OpenBracket,
        Kind::CloseBracket => token::CloseBracket,
        Kind::Ident => token::Ident(
            Symbol::new(t.data),
            if t.is_raw() { IdentIsRaw::Yes } else { IdentIsRaw::No },
        ),
        Kind::Lifetime => token::Lifetime(Symbol::new(t.data), IdentIsRaw::No),
        Kind::LineDoc | Kind::BlockDoc => token::DocComment(
            if t.kind == Kind::LineDoc { CommentKind::Line } else { CommentKind::Block },
            if t.aux == 1 { AttrStyle::Inner } else { AttrStyle::Outer },
            Symbol::new(t.data),
        ),
        Kind::Integer => token::Literal(token::Lit::new(
            token::Integer,
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::Float => token::Literal(token::Lit::new(
            token::Float,
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::Char => token::Literal(token::Lit::new(
            token::Char,
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::Byte => token::Literal(token::Lit::new(
            token::Byte,
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::Str => token::Literal(token::Lit::new(
            token::Str,
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::ByteStr => token::Literal(token::Lit::new(
            token::ByteStr,
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::CStr => token::Literal(token::Lit::new(
            token::CStr,
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::StrRaw => token::Literal(token::Lit::new(
            token::StrRaw(t.aux),
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::ByteStrRaw => token::Literal(token::Lit::new(
            token::ByteStrRaw(t.aux),
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::CStrRaw => token::Literal(token::Lit::new(
            token::CStrRaw(t.aux),
            Symbol::new(t.data),
            suffix(t, suffixes, index),
        )),
        Kind::Eof => unreachable!("EOF handled before span construction"),
    };
    (Token::new(kind, span), spacing)
}

#[inline(always)]
fn suffix(t: &LexedToken, suffixes: &[(u32, Symbol)], index: u32) -> Option<Symbol> {
    if !t.has_suffix() {
        return None;
    }
    suffixes.binary_search_by_key(&index, |&(i, _)| i).ok().map(|at| suffixes[at].1)
}
