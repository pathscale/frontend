// This module is wired into the source cursor by the following queue parts.
#![allow(dead_code)]

use crate::rustc_ast::token::{self, Delimiter, TokenKind};

use super::Spacing;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(super) enum SourceKind {
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
    Ident,
    Lifetime,
    Integer,
    Float,
    Char,
    Byte,
    Str,
    ByteStr,
    CStr,
    StrRaw,
    ByteStrRaw,
    CStrRaw,
    LineDoc,
    BlockDoc,
    Eof,
}

impl SourceKind {
    pub(super) fn is_punct(self) -> bool {
        self <= Self::Question
    }

    pub(super) fn open_delim(self) -> Option<Delimiter> {
        match self {
            Self::OpenParen => Some(Delimiter::Parenthesis),
            Self::OpenBrace => Some(Delimiter::Brace),
            Self::OpenBracket => Some(Delimiter::Bracket),
            _ => None,
        }
    }

    pub(super) fn close_delim(self) -> Option<Delimiter> {
        match self {
            Self::CloseParen => Some(Delimiter::Parenthesis),
            Self::CloseBrace => Some(Delimiter::Brace),
            Self::CloseBracket => Some(Delimiter::Bracket),
            _ => None,
        }
    }

    pub(super) fn simple_kind(self) -> Option<TokenKind> {
        Some(match self {
            Self::Eq => token::Eq,
            Self::Lt => token::Lt,
            Self::Le => token::Le,
            Self::EqEq => token::EqEq,
            Self::Ne => token::Ne,
            Self::Ge => token::Ge,
            Self::Gt => token::Gt,
            Self::AndAnd => token::AndAnd,
            Self::OrOr => token::OrOr,
            Self::Bang => token::Bang,
            Self::Tilde => token::Tilde,
            Self::Plus => token::Plus,
            Self::Minus => token::Minus,
            Self::Star => token::Star,
            Self::Slash => token::Slash,
            Self::Percent => token::Percent,
            Self::Caret => token::Caret,
            Self::And => token::And,
            Self::Or => token::Or,
            Self::Shl => token::Shl,
            Self::Shr => token::Shr,
            Self::PlusEq => token::PlusEq,
            Self::MinusEq => token::MinusEq,
            Self::StarEq => token::StarEq,
            Self::SlashEq => token::SlashEq,
            Self::PercentEq => token::PercentEq,
            Self::CaretEq => token::CaretEq,
            Self::AndEq => token::AndEq,
            Self::OrEq => token::OrEq,
            Self::ShlEq => token::ShlEq,
            Self::ShrEq => token::ShrEq,
            Self::At => token::At,
            Self::Dot => token::Dot,
            Self::DotDot => token::DotDot,
            Self::DotDotDot => token::DotDotDot,
            Self::DotDotEq => token::DotDotEq,
            Self::Comma => token::Comma,
            Self::Semi => token::Semi,
            Self::Colon => token::Colon,
            Self::PathSep => token::PathSep,
            Self::RArrow => token::RArrow,
            Self::LArrow => token::LArrow,
            Self::FatArrow => token::FatArrow,
            Self::Pound => token::Pound,
            Self::Dollar => token::Dollar,
            Self::Question => token::Question,
            Self::OpenParen => token::OpenParen,
            Self::CloseParen => token::CloseParen,
            Self::OpenBrace => token::OpenBrace,
            Self::CloseBrace => token::CloseBrace,
            Self::OpenBracket => token::OpenBracket,
            Self::CloseBracket => token::CloseBracket,
            Self::Eof => token::Eof,
            _ => return None,
        })
    }

    pub(super) fn glue(self, next: Self) -> Option<Self> {
        use SourceKind::*;
        Some(match (self, next) {
            (Eq, Eq) => EqEq,
            (Eq, Gt) => FatArrow,
            (Lt, Eq) => Le,
            (Lt, Lt) => Shl,
            (Lt, Le) => ShlEq,
            (Lt, Minus) => LArrow,
            (Gt, Eq) => Ge,
            (Gt, Gt) => Shr,
            (Gt, Ge) => ShrEq,
            (Bang, Eq) => Ne,
            (Plus, Eq) => PlusEq,
            (Minus, Eq) => MinusEq,
            (Minus, Gt) => RArrow,
            (Star, Eq) => StarEq,
            (Slash, Eq) => SlashEq,
            (Percent, Eq) => PercentEq,
            (Caret, Eq) => CaretEq,
            (And, Eq) => AndEq,
            (And, And) => AndAnd,
            (Or, Eq) => OrEq,
            (Or, Or) => OrOr,
            (Shl, Eq) => ShlEq,
            (Shr, Eq) => ShrEq,
            (Dot, Dot) => DotDot,
            (Dot, DotDot) => DotDotDot,
            (DotDot, Dot) => DotDotDot,
            (DotDot, Eq) => DotDotEq,
            (Colon, Colon) => PathSep,
            _ => return None,
        })
    }
}

/// A source lexeme has no symbols, interned span, or macro-only payload.
/// `suffix` is relative to `lo`; `aux` is a raw-string hash count or doc style.
#[derive(Clone, Copy, Debug)]
pub(super) struct SourceToken {
    pub(super) lo: u32,
    pub(super) hi: u32,
    pub(super) suffix: u32,
    pub(super) kind: SourceKind,
    pub(super) aux: u8,
    pub(super) spacing: Spacing,
}

const _: () = assert!(core::mem::size_of::<SourceToken>() == 16);

/// An exact key, including length, because source identifiers contain no NUL.
pub(super) fn short_key(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    Some(match bytes.len() {
        1 => u64::from(bytes[0]),
        2..=3 => {
            let first = u16::from_le_bytes(bytes[..2].try_into().unwrap());
            u64::from(first) | (u64::from(bytes[bytes.len() - 1]) << ((bytes.len() - 1) * 8))
        }
        4..=8 => {
            let first = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            let last = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
            u64::from(first) | (u64::from(last) << ((bytes.len() - 4) * 8))
        }
        _ => return None,
    })
}

pub(super) fn short_slot(key: u64) -> usize {
    (key.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 55) as usize
}
