use crate::rustc_ast::token::{self, Delimiter, TokenKind};

use super::Spacing;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum SourceKind {
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "retains source-kind decoding for serialized tapes")
    )]
    pub(crate) const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Eq,
            1 => Self::Lt,
            2 => Self::Le,
            3 => Self::EqEq,
            4 => Self::Ne,
            5 => Self::Ge,
            6 => Self::Gt,
            7 => Self::AndAnd,
            8 => Self::OrOr,
            9 => Self::Bang,
            10 => Self::Tilde,
            11 => Self::Plus,
            12 => Self::Minus,
            13 => Self::Star,
            14 => Self::Slash,
            15 => Self::Percent,
            16 => Self::Caret,
            17 => Self::And,
            18 => Self::Or,
            19 => Self::Shl,
            20 => Self::Shr,
            21 => Self::PlusEq,
            22 => Self::MinusEq,
            23 => Self::StarEq,
            24 => Self::SlashEq,
            25 => Self::PercentEq,
            26 => Self::CaretEq,
            27 => Self::AndEq,
            28 => Self::OrEq,
            29 => Self::ShlEq,
            30 => Self::ShrEq,
            31 => Self::At,
            32 => Self::Dot,
            33 => Self::DotDot,
            34 => Self::DotDotDot,
            35 => Self::DotDotEq,
            36 => Self::Comma,
            37 => Self::Semi,
            38 => Self::Colon,
            39 => Self::PathSep,
            40 => Self::RArrow,
            41 => Self::LArrow,
            42 => Self::FatArrow,
            43 => Self::Pound,
            44 => Self::Dollar,
            45 => Self::Question,
            46 => Self::OpenParen,
            47 => Self::CloseParen,
            48 => Self::OpenBrace,
            49 => Self::CloseBrace,
            50 => Self::OpenBracket,
            51 => Self::CloseBracket,
            52 => Self::Ident,
            53 => Self::Lifetime,
            54 => Self::Integer,
            55 => Self::Float,
            56 => Self::Char,
            57 => Self::Byte,
            58 => Self::Str,
            59 => Self::ByteStr,
            60 => Self::CStr,
            61 => Self::StrRaw,
            62 => Self::ByteStrRaw,
            63 => Self::CStrRaw,
            64 => Self::LineDoc,
            65 => Self::BlockDoc,
            66 => Self::Eof,
            _ => Self::Eof,
        }
    }

    pub(super) fn is_punct(self) -> bool {
        self <= Self::Question
    }

    pub(crate) const fn is_open_delim(self) -> Option<Delimiter> {
        self.open_delim()
    }

    pub(crate) const fn is_close_delim(self) -> Option<Delimiter> {
        self.close_delim()
    }

    #[inline]
    pub(crate) const fn is_literal(self) -> bool {
        matches!(
            self,
            Self::Integer
                | Self::Float
                | Self::Char
                | Self::Byte
                | Self::Str
                | Self::ByteStr
                | Self::CStr
                | Self::StrRaw
                | Self::ByteStrRaw
                | Self::CStrRaw
        )
    }

    #[cfg(feature = "parallel")]
    pub(crate) const fn needs_symbol_lookup(self) -> bool {
        matches!(self, Self::Ident | Self::Lifetime | Self::LineDoc | Self::BlockDoc)
    }

    pub(super) const fn open_delim(self) -> Option<Delimiter> {
        match self {
            Self::OpenParen => Some(Delimiter::Parenthesis),
            Self::OpenBrace => Some(Delimiter::Brace),
            Self::OpenBracket => Some(Delimiter::Bracket),
            _ => None,
        }
    }

    pub(super) const fn close_delim(self) -> Option<Delimiter> {
        match self {
            Self::CloseParen => Some(Delimiter::Parenthesis),
            Self::CloseBrace => Some(Delimiter::Brace),
            Self::CloseBracket => Some(Delimiter::Bracket),
            _ => None,
        }
    }

    #[inline]
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
/// `suffix` is relative to `lo`; `aux` stores raw-string hashes, doc style, or raw-ident state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceToken {
    pub(crate) lo: u32,
    pub(crate) hi: u32,
    pub(crate) suffix: u32,
    pub(crate) kind: SourceKind,
    pub(crate) aux: u8,
    pub(crate) spacing: Spacing,
}

const _: () = assert!(core::mem::size_of::<SourceToken>() == 16);

/// An exact key, including length, because source identifiers contain no NUL.
#[inline]
pub(crate) fn short_key(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > 8 || bytes.contains(&0) {
        return None;
    }
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
        _ => unreachable!("short key length checked"),
    })
}

#[inline]
pub(crate) fn short_slot(key: u64) -> usize {
    (key.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 55) as usize
}
