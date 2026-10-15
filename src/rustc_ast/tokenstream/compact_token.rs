use super::Spacing;
use super::source_token::SourceKind;

pub(crate) const COMPACT_LEN_MASK: u16 = 0x0FFF;
pub(crate) const COMPACT_LEN_OVERFLOW: u16 = 0x0FFF;
pub(crate) const COMPACT_FLAG_JOINT: u16 = 1 << 12;
pub(crate) const COMPACT_FLAG_JOINT_HIDDEN: u16 = 1 << 13;
pub(crate) const COMPACT_FLAG_HAS_GROUP: u16 = 1 << 14;
pub(crate) const COMPACT_FLAG_HAS_AUX_U32: u16 = 1 << 15;

/// Length overflow, group IDs and u32 payloads live in independent tape arrays.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompactToken {
    pub(crate) lo: u32,
    pub(crate) len_and_flags: u16,
    pub(crate) kind: SourceKind,
    pub(crate) aux: u8,
}

const _: () = assert!(core::mem::size_of::<CompactToken>() == 8);
const _: () = assert!(core::mem::align_of::<CompactToken>() == 8);

impl CompactToken {
    #[inline]
    pub(crate) const fn new(
        lo: u32,
        len: u32,
        kind: SourceKind,
        spacing: Spacing,
        aux: u8,
        has_group: bool,
        has_aux_u32: bool,
    ) -> Self {
        let mut len_and_flags = if len < COMPACT_LEN_OVERFLOW as u32 {
            len as u16
        } else {
            COMPACT_LEN_OVERFLOW
        };
        len_and_flags |= match spacing {
            Spacing::Alone => 0,
            Spacing::Joint => COMPACT_FLAG_JOINT,
            Spacing::JointHidden => COMPACT_FLAG_JOINT_HIDDEN,
        };
        if has_group {
            len_and_flags |= COMPACT_FLAG_HAS_GROUP;
        }
        if has_aux_u32 {
            len_and_flags |= COMPACT_FLAG_HAS_AUX_U32;
        }
        Self { lo, len_and_flags, kind, aux }
    }

    #[inline]
    pub(crate) const fn lo(self) -> u32 {
        self.lo
    }

    #[inline]
    pub(crate) const fn inline_len(self) -> Option<u32> {
        let len = self.len_and_flags & COMPACT_LEN_MASK;
        if len == COMPACT_LEN_OVERFLOW { None } else { Some(len as u32) }
    }

    #[inline]
    pub(crate) const fn kind(self) -> SourceKind {
        self.kind
    }

    #[inline]
    pub(crate) const fn aux(self) -> u8 {
        self.aux
    }

    #[inline]
    pub(crate) const fn spacing(self) -> Spacing {
        if self.len_and_flags & COMPACT_FLAG_JOINT != 0 {
            Spacing::Joint
        } else if self.len_and_flags & COMPACT_FLAG_JOINT_HIDDEN != 0 {
            Spacing::JointHidden
        } else {
            Spacing::Alone
        }
    }

    #[inline]
    pub(crate) const fn has_group(self) -> bool {
        self.len_and_flags & COMPACT_FLAG_HAS_GROUP != 0
    }

    #[inline]
    pub(crate) const fn has_aux_u32(self) -> bool {
        self.len_and_flags & COMPACT_FLAG_HAS_AUX_U32 != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreGluedPunct {
    pub(crate) kind: SourceKind,
    pub(crate) len: u8,
    pub(crate) spacing: Spacing,
}

/// A punctuation lexeme, excluding delimiters and complete comment openers.
#[inline]
fn single_punct(bytes: &[u8]) -> Option<SourceKind> {
    use SourceKind::*;
    Some(match *bytes.first()? {
        b'=' => Eq,
        b'<' => Lt,
        b'>' => Gt,
        b'!' => Bang,
        b'~' => Tilde,
        b'+' => Plus,
        b'-' => Minus,
        b'*' => Star,
        b'/' if !matches!(bytes.get(1), Some(b'/' | b'*')) => Slash,
        b'%' => Percent,
        b'^' => Caret,
        b'&' => And,
        b'|' => Or,
        b'@' => At,
        b'.' => Dot,
        b',' => Comma,
        b';' => Semi,
        b':' => Colon,
        b'#' => Pound,
        b'$' => Dollar,
        b'?' => Question,
        _ => return None,
    })
}

/// Whether the very next lexeme is punctuation, even if it cannot actually glue.
#[inline]
pub(crate) fn may_glue_after(bytes: &[u8], consumed: usize) -> bool {
    bytes.get(consumed..).and_then(single_punct).is_some()
}

#[inline]
fn spacing_after(bytes: &[u8], consumed: usize) -> Spacing {
    let Some(tail) = bytes.get(consumed..) else { return Spacing::Alone };
    // Pattern_White_Space, including the five non-ASCII whitespace codepoints.
    if tail.is_empty()
        || matches!(tail[0], b'\t'..=b'\r' | b' ')
        || tail.starts_with(b"\xC2\x85")
        || matches!(tail, [0xE2, 0x80, 0x8E | 0x8F | 0xA8 | 0xA9, ..])
    {
        return Spacing::Alone;
    }
    if tail.starts_with(b"//") {
        // Four or more slashes start an ordinary comment, not an outer doc.
        return if tail.starts_with(b"//!")
            || (tail.starts_with(b"///") && tail.get(3) != Some(&b'/'))
        {
            Spacing::JointHidden
        } else {
            Spacing::Alone
        };
    }
    if tail.starts_with(b"/*") {
        // /**/ and /*** are ordinary comments; /*! is always an inner doc.
        return if tail.starts_with(b"/*!")
            || (tail.starts_with(b"/**") && !matches!(tail.get(3), Some(b'*' | b'/')))
        {
            Spacing::JointHidden
        } else {
            Spacing::Alone
        };
    }
    if may_glue_after(bytes, consumed) { Spacing::Joint } else { Spacing::JointHidden }
}

/// Longest punctuation prefix under SourceKind::glue, without eating comments.
#[inline]
pub(crate) fn try_glue_punct(bytes: &[u8]) -> Option<PreGluedPunct> {
    use SourceKind::*;
    let (kind, len) = match bytes {
        [b'.', b'.', b'=', ..] => (DotDotEq, 3),
        [b'.', b'.', b'.', ..] => (DotDotDot, 3),
        [b'<', b'<', b'=', ..] => (ShlEq, 3),
        [b'>', b'>', b'=', ..] => (ShrEq, 3),
        [b'<', b'-', ..] => (LArrow, 2),
        [b'-', b'>', ..] => (RArrow, 2),
        [b'=', b'>', ..] => (FatArrow, 2),
        [b':', b':', ..] => (PathSep, 2),
        [b'.', b'.', ..] => (DotDot, 2),
        [b'=', b'=', ..] => (EqEq, 2),
        [b'!', b'=', ..] => (Ne, 2),
        [b'<', b'=', ..] => (Le, 2),
        [b'>', b'=', ..] => (Ge, 2),
        [b'&', b'&', ..] => (AndAnd, 2),
        [b'|', b'|', ..] => (OrOr, 2),
        [b'<', b'<', ..] => (Shl, 2),
        [b'>', b'>', ..] => (Shr, 2),
        [b'+', b'=', ..] => (PlusEq, 2),
        [b'-', b'=', ..] => (MinusEq, 2),
        [b'*', b'=', ..] => (StarEq, 2),
        [b'/', b'=', ..] => (SlashEq, 2),
        [b'%', b'=', ..] => (PercentEq, 2),
        [b'^', b'=', ..] => (CaretEq, 2),
        [b'&', b'=', ..] => (AndEq, 2),
        [b'|', b'=', ..] => (OrEq, 2),
        _ => (single_punct(bytes)?, 1),
    };
    Some(PreGluedPunct { kind, len, spacing: spacing_after(bytes, len as usize) })
}
