//! Source-relative structural metadata collected in one forward lexical pass.

use alloc::vec::Vec;

use super::simd_classify::{
    BLOCK_LEN, classify_block, classify_partial, low_bits_mask, mask_newline_and_bidi_64,
};
use super::simd_scan::is_text_flow_control_at;
use super::{Cursor, FrontmatterAllowed, LiteralKind, TokenKind, is_whitespace};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelimKind {
    Paren,
    Brace,
    Bracket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StructuralGroup {
    pub delim: DelimKind,
    pub open_byte: u32,
    pub close_byte: u32,
    pub parent: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StructuralIndex {
    pub newlines: Vec<u32>,
    pub groups: Vec<StructuralGroup>,
    pub depth_zero_cuts: Vec<u32>,
    pub balanced: bool,
    pub has_bidi_or_non_ascii_ws: bool,
}

/// Return every LF byte position, including those inside comments and literals.
///
/// Panics if the source length cannot be represented by a `u32` byte offset.
pub fn scan_newline_offsets(src: &str) -> Vec<u32> {
    assert_source_len(src);
    let mut newlines = Vec::new();
    for (block_idx, chunk) in src.as_bytes().chunks(BLOCK_LEN).enumerate() {
        let base = block_idx * BLOCK_LEN;
        let mask = if let Ok(block) = <&[u8; BLOCK_LEN]>::try_from(chunk) {
            mask_newline_and_bidi_64(block).0
        } else {
            // Zero padding introduces no LF bytes and never reads outside the tail.
            let mut block = [0; BLOCK_LEN];
            block[..chunk.len()].copy_from_slice(chunk);
            mask_newline_and_bidi_64(&block).0 & low_bits_mask(chunk.len())
        };
        append_offsets(&mut newlines, base, mask);
    }
    newlines
}

/// Index delimiters and cuts immediately after top-level `;` and `}` bytes.
///
/// SIMD blocks supply structural events and full-source LF/Unicode metadata.
/// Cursor handles other lexemes so literal prefixes, suffixes, nested comments,
/// and lifetime-versus-character decisions stay identical to the lexer. Its
/// consumed range masks events in this block and all following covered blocks.
/// There is no preliminary newline scan or second tokenization pass.
///
/// Unusable input retains its newline/Unicode metadata but discards all groups
/// and cuts. Panics if the source length cannot fit in a `u32` byte offset.
pub fn build_structural_index(src: &str) -> StructuralIndex {
    assert_source_len(src);
    let bytes = src.as_bytes();
    let mut index = StructuralIndex { balanced: true, ..StructuralIndex::default() };
    let mut stack = Vec::new();
    let mut consumed_until = 0;

    for (block_idx, chunk) in bytes.chunks(BLOCK_LEN).enumerate() {
        let base = block_idx * BLOCK_LEN;
        let end = base + chunk.len();
        let masks = if let Ok(block) = <&[u8; BLOCK_LEN]>::try_from(chunk) {
            classify_block(block)
        } else {
            classify_partial(chunk)
        };
        append_offsets(&mut index.newlines, base, masks.newline);

        // Inspect the full source at each UTF-8 lead, including leads whose
        // codepoint crosses a block boundary or lies in a skipped lexeme.
        if !index.has_bidi_or_non_ascii_ws {
            let mut non_ascii = masks.non_ascii;
            while non_ascii != 0 {
                let offset = base + non_ascii.trailing_zeros() as usize;
                non_ascii &= non_ascii - 1;
                if bytes[offset] >= 0xC0 {
                    // Valid UTF-8 makes every lead byte a character boundary.
                    let ch = src[offset..].chars().next().unwrap();
                    if is_whitespace(ch) || is_text_flow_control_at(bytes, offset) {
                        index.has_bidi_or_non_ascii_ws = true;
                        break;
                    }
                }
            }
        }

        if consumed_until >= end {
            continue;
        }
        let structural = masks.open_paren
            | masks.close_paren
            | masks.open_brace
            | masks.close_brace
            | masks.open_bracket
            | masks.close_bracket
            | masks.semi;
        let mut events = low_bits_mask(chunk.len())
            & !masks.whitespace
            & !low_bits_mask(consumed_until.saturating_sub(base));

        while events != 0 {
            let lane = events.trailing_zeros() as usize;
            let offset = base + lane;
            if structural & (1u64 << lane) != 0 {
                match bytes[offset] {
                    b'(' => open_group(&mut index, &mut stack, DelimKind::Paren, offset),
                    b'{' => open_group(&mut index, &mut stack, DelimKind::Brace, offset),
                    b'[' => open_group(&mut index, &mut stack, DelimKind::Bracket, offset),
                    b')' => close_group(&mut index, &mut stack, DelimKind::Paren, offset),
                    b'}' => close_group(&mut index, &mut stack, DelimKind::Brace, offset),
                    b']' => close_group(&mut index, &mut stack, DelimKind::Bracket, offset),
                    b';' if stack.is_empty() => index.depth_zero_cuts.push((offset + 1) as u32),
                    b';' => {}
                    _ => unreachable!("structural masks only contain delimiters and semicolons"),
                }
                consumed_until = offset + 1;
            } else {
                // Events outside skipped ranges are on token/UTF-8 boundaries:
                // ASCII whitespace and structural bytes are single-byte tokens,
                // and every other event advances by a complete Cursor token.
                let token = Cursor::new(&src[offset..], FrontmatterAllowed::No).advance_token();
                consumed_until = offset + token.len as usize;
                if !lexeme_terminated(token.kind) {
                    index.balanced = false;
                }
            }
            events &= !low_bits_mask(consumed_until - base);
        }
    }

    if !stack.is_empty() {
        index.balanced = false;
    }
    if !index.balanced {
        // Never expose placeholders or parent links into an incomplete table.
        index.groups.clear();
        index.depth_zero_cuts.clear();
    }
    index
}

#[inline]
fn assert_source_len(src: &str) {
    assert!(u32::try_from(src.len()).is_ok(), "structural source exceeds u32 byte offsets");
}

#[inline]
fn append_offsets(offsets: &mut Vec<u32>, base: usize, mut mask: u64) {
    while mask != 0 {
        offsets.push((base + mask.trailing_zeros() as usize) as u32);
        mask &= mask - 1;
    }
}

#[inline]
fn open_group(index: &mut StructuralIndex, stack: &mut Vec<u32>, delim: DelimKind, offset: usize) {
    // Each group consumes a distinct source byte, so the checked source length
    // also keeps every opening-order ID below the reserved root sentinel.
    let group_id = index.groups.len() as u32;
    index.groups.push(StructuralGroup {
        delim,
        open_byte: offset as u32,
        close_byte: u32::MAX,
        parent: stack.last().copied().unwrap_or(u32::MAX),
    });
    stack.push(group_id);
}

#[inline]
fn close_group(index: &mut StructuralIndex, stack: &mut Vec<u32>, delim: DelimKind, offset: usize) {
    let Some(group_id) = stack.pop() else {
        index.balanced = false;
        return;
    };
    let group = &mut index.groups[group_id as usize];
    if group.delim != delim {
        index.balanced = false;
        return;
    }
    group.close_byte = offset as u32;
    if delim == DelimKind::Brace && stack.is_empty() {
        index.depth_zero_cuts.push((offset + 1) as u32);
    }
}

#[inline]
fn lexeme_terminated(kind: TokenKind) -> bool {
    !matches!(
        kind,
        TokenKind::BlockComment { terminated: false, .. }
            | TokenKind::Literal {
                kind: LiteralKind::Char { terminated: false }
                    | LiteralKind::Byte { terminated: false }
                    | LiteralKind::Str { terminated: false }
                    | LiteralKind::ByteStr { terminated: false }
                    | LiteralKind::CStr { terminated: false }
                    | LiteralKind::RawStr { n_hashes: None }
                    | LiteralKind::RawByteStr { n_hashes: None }
                    | LiteralKind::RawCStr { n_hashes: None },
                ..
            }
    )
}
