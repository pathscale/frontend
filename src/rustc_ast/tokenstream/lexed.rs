//! A `frontend_lex::LexedFile` read as parser tokens.
//!
//! The lexer has already interned every identifier, lifetime, literal and doc comment, so a
//! token here is built from the file's arrays alone: no source text is read and nothing is
//! interned. Spans are the token's offsets plus the file's position in the source map.

use frontend_lex::{Kind, LexedFile};

use super::Spacing;
use crate::rustc_ast::ast::AttrStyle;
use crate::rustc_ast::token::{self, CommentKind, IdentIsRaw, Token, TokenKind};
use crate::rustc_span::{BytePos, DUMMY_SP, Span, Symbol};

/// The parser's kind for a token with no payload, or `None` for one that carries a symbol.
#[inline]
pub(crate) fn simple_kind(kind: Kind) -> Option<TokenKind> {
    Some(match kind {
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
        Kind::Eof => token::Eof,
        _ => return None,
    })
}

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

/// Token `index` of `file`, as the parser reads it, for a file that starts at `base`.
#[inline]
pub(crate) fn token(file: &LexedFile, base: BytePos, index: usize) -> (Token, Spacing) {
    let t = &file.tokens[index];
    let spacing = spacing(t.spacing());
    if t.kind == Kind::Eof {
        return (Token::new(token::Eof, DUMMY_SP), spacing);
    }
    let span = span(base, t.lo, t.hi);
    if let Some(simple) = simple_kind(t.kind) {
        return (Token::new(simple, span), spacing);
    }
    let symbol = Symbol::new(t.data);
    let kind = match t.kind {
        Kind::Ident => {
            token::Ident(symbol, if t.is_raw() { IdentIsRaw::Yes } else { IdentIsRaw::No })
        }
        Kind::Lifetime => token::Lifetime(symbol, IdentIsRaw::No),
        Kind::LineDoc | Kind::BlockDoc => token::DocComment(
            if t.kind == Kind::LineDoc { CommentKind::Line } else { CommentKind::Block },
            if t.aux == 1 { AttrStyle::Inner } else { AttrStyle::Outer },
            symbol,
        ),
        literal => {
            let kind = match literal {
                Kind::Integer => token::Integer,
                Kind::Float => token::Float,
                Kind::Char => token::Char,
                Kind::Byte => token::Byte,
                Kind::Str => token::Str,
                Kind::ByteStr => token::ByteStr,
                Kind::CStr => token::CStr,
                Kind::StrRaw => token::StrRaw(t.aux),
                Kind::ByteStrRaw => token::ByteStrRaw(t.aux),
                Kind::CStrRaw => token::CStrRaw(t.aux),
                other => unreachable!("{other:?} has a simple kind"),
            };
            token::Literal(token::Lit::new(kind, symbol, file.suffix(index as u32)))
        }
    };
    (Token::new(kind, span), spacing)
}
