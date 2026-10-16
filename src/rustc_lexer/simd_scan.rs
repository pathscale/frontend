//! Byte scanners with opener-relative lengths and bounded SIMD classification.

use alloc::vec::Vec;
use core::convert::{TryFrom, TryInto};

use super::simd_classify::{
    self, BLOCK_LEN, BlockMasks, classify_block, classify_partial, low_bits_mask,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineCommentScan {
    pub len: usize,
    pub has_bidi: bool,
    pub has_cr: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockCommentScan {
    pub len: usize,
    pub terminated: bool,
    pub has_bidi: bool,
    pub has_cr: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringBodyScan {
    pub len: usize,
    pub terminated: bool,
    pub clean_ascii_no_escape: bool,
    pub has_bidi: bool,
    pub has_cr: bool,
}

/// Recognize only complete UTF-8 encodings of text flow control characters.
#[inline]
pub fn is_text_flow_control_at(bytes: &[u8], idx: usize) -> bool {
    matches!(
        bytes.get(idx..),
        Some([0xE2, 0x80, 0x8E | 0x8F | 0xAA..=0xAE, ..])
            | Some([0xE2, 0x81, 0xA6..=0xA9, ..])
            | Some([0xD8, 0x9C, ..])
    )
}

/// Candidate lanes refer to the original slice, so UTF-8 can cross a block edge.
#[inline]
fn bidi_in_mask(bytes: &[u8], offset: usize, mut candidates: u64) -> bool {
    while candidates != 0 {
        let lane = candidates.trailing_zeros() as usize;
        if is_text_flow_control_at(bytes, offset + lane) {
            return true;
        }
        candidates &= candidates - 1;
    }
    false
}

#[inline]
fn masks_at(bytes: &[u8], offset: usize) -> (BlockMasks, usize) {
    let len = (bytes.len() - offset).min(BLOCK_LEN);
    let block = &bytes[offset..offset + len];
    let masks = if len == BLOCK_LEN {
        classify_block(block.try_into().expect("full classification block"))
    } else {
        classify_partial(block)
    };
    (masks, len)
}

#[inline]
fn record_newlines(output: &mut Option<(u32, &mut Vec<u32>)>, offset: usize, mut lines: u64) {
    let Some((base_offset, newlines)) = output.as_mut() else {
        return;
    };
    while lines != 0 {
        let relative = offset + lines.trailing_zeros() as usize;
        let relative = u32::try_from(relative).expect("newline offset exceeds u32");
        newlines.push(base_offset.checked_add(relative).expect("newline offset exceeds u32"));
        lines &= lines - 1;
    }
}

pub fn contains_text_flow_control_char(bytes: &[u8]) -> bool {
    let mut offset = 0;
    while offset < bytes.len() {
        let len = (bytes.len() - offset).min(BLOCK_LEN);
        let block = &bytes[offset..offset + len];
        let candidates = if len == BLOCK_LEN {
            simd_classify::mask_newline_and_bidi_64(block.try_into().expect("full bidi block")).1
        } else {
            classify_partial(block).bidi_lead
        };
        if bidi_in_mask(bytes, offset, candidates) {
            return true;
        }
        offset += len;
    }
    false
}

pub fn scan_ascii_whitespace(bytes: &[u8]) -> usize {
    let mut offset = 0;
    while offset < simd_classify::LEADING_WORD_BYTES
        && let Some(word) = bytes.get(offset..offset + 8)
    {
        let word = u64::from_le_bytes(word.try_into().expect("eight-byte whitespace word"));
        if let Some(lane) = simd_classify::first_rejected_lane(simd_classify::word_whitespace(word))
        {
            return offset + lane;
        }
        offset += 8;
    }
    while offset < bytes.len() {
        let len = (bytes.len() - offset).min(BLOCK_LEN);
        let block = &bytes[offset..offset + len];
        let whitespace = if len == BLOCK_LEN {
            simd_classify::mask_ascii_whitespace_64(
                block.try_into().expect("full whitespace block"),
            )
            .0
        } else {
            classify_partial(block).whitespace
        };
        let stop = !whitespace & low_bits_mask(len);
        if stop != 0 {
            return offset + stop.trailing_zeros() as usize;
        }
        offset += len;
    }
    offset
}

/// Append absolute byte positions of LF bytes in the consumed whitespace.
pub fn scan_ascii_whitespace_with_newlines(
    bytes: &[u8],
    base_offset: u32,
    newlines: &mut Vec<u32>,
) -> usize {
    let mut offset = 0;
    let mut output = Some((base_offset, newlines));
    while offset < bytes.len() {
        let (masks, len) = masks_at(bytes, offset);
        let stop = !masks.whitespace & low_bits_mask(len);
        let consumed = if stop == 0 { len } else { stop.trailing_zeros() as usize };
        record_newlines(&mut output, offset, masks.newline & low_bits_mask(consumed));
        offset += consumed;
        if consumed != len {
            break;
        }
    }
    offset
}

pub fn scan_line_comment(bytes: &[u8]) -> LineCommentScan {
    let mut offset = 0;
    let mut has_bidi = false;
    let mut has_cr = false;
    while offset < bytes.len() {
        let (masks, len) = masks_at(bytes, offset);
        let consumed =
            if masks.newline == 0 { len } else { masks.newline.trailing_zeros() as usize };
        let active = low_bits_mask(consumed);
        has_bidi |= bidi_in_mask(bytes, offset, masks.bidi_lead & active);
        has_cr |= masks.cr & active != 0;
        offset += consumed;
        if consumed != len {
            break;
        }
    }
    LineCommentScan { len: offset, has_bidi, has_cr }
}

pub fn scan_block_comment(bytes: &[u8]) -> BlockCommentScan {
    scan_block_comment_inner(bytes, None)
}

/// Append absolute LF positions while scanning a block comment body.
pub fn scan_block_comment_with_newlines(
    bytes: &[u8],
    base_offset: u32,
    newlines: &mut Vec<u32>,
) -> BlockCommentScan {
    scan_block_comment_inner(bytes, Some((base_offset, newlines)))
}

fn scan_block_comment_inner(
    bytes: &[u8],
    mut output: Option<(u32, &mut Vec<u32>)>,
) -> BlockCommentScan {
    let mut offset = 0;
    let mut depth = 1usize;
    let mut has_bidi = false;
    let mut has_cr = false;
    while offset < bytes.len() {
        let (masks, len) = masks_at(bytes, offset);
        let mut delimiters = masks.slash | masks.star;
        let mut consumed = len;
        while delimiters != 0 {
            let lane = delimiters.trailing_zeros() as usize;
            let idx = offset + lane;
            match (bytes[idx], bytes.get(idx + 1)) {
                (b'/', Some(b'*')) => depth += 1,
                (b'*', Some(b'/')) => {
                    depth -= 1;
                    if depth == 0 {
                        let end = idx + 2;
                        let active = low_bits_mask(end - offset);
                        has_bidi |= bidi_in_mask(bytes, offset, masks.bidi_lead & active);
                        has_cr |= masks.cr & active != 0;
                        record_newlines(&mut output, offset, masks.newline & active);
                        return BlockCommentScan { len: end, terminated: true, has_bidi, has_cr };
                    }
                }
                _ => {
                    delimiters &= delimiters - 1;
                    continue;
                }
            }
            // Consume both delimiter bytes, including a second byte in the next block.
            consumed = consumed.max(lane + 2);
            delimiters &= !low_bits_mask(lane + 2);
        }
        has_bidi |= bidi_in_mask(bytes, offset, masks.bidi_lead);
        has_cr |= masks.cr != 0;
        record_newlines(&mut output, offset, masks.newline);
        offset += consumed;
    }
    BlockCommentScan { len: bytes.len(), terminated: false, has_bidi, has_cr }
}

pub fn scan_double_quoted_string(bytes: &[u8]) -> StringBodyScan {
    scan_double_quoted_string_inner(bytes, None)
}

/// Append absolute LF positions, including escaped LF, in the consumed body.
pub fn scan_double_quoted_string_with_newlines(
    bytes: &[u8],
    base_offset: u32,
    newlines: &mut Vec<u32>,
) -> StringBodyScan {
    scan_double_quoted_string_inner(bytes, Some((base_offset, newlines)))
}

fn scan_double_quoted_string_inner(
    bytes: &[u8],
    mut output: Option<(u32, &mut Vec<u32>)>,
) -> StringBodyScan {
    let mut offset = 0;
    let mut clean_ascii_no_escape = true;
    let mut has_bidi = false;
    let mut has_cr = false;
    while offset < bytes.len() {
        let (masks, len) = masks_at(bytes, offset);
        let events = masks.quote | masks.backslash;
        let consumed = if events == 0 { len } else { events.trailing_zeros() as usize + 1 };
        let active = low_bits_mask(consumed);
        clean_ascii_no_escape &= masks.non_ascii & active == 0;
        has_cr |= masks.cr & active != 0;
        has_bidi |= bidi_in_mask(bytes, offset, masks.bidi_lead & active);
        record_newlines(&mut output, offset, masks.newline & active);
        offset += consumed;
        if events == 0 {
            continue;
        }
        if bytes[offset - 1] == b'"' {
            return StringBodyScan {
                len: offset,
                terminated: true,
                clean_ascii_no_escape,
                has_bidi,
                has_cr,
            };
        }
        clean_ascii_no_escape = false;
        if let Some(&escaped) = bytes.get(offset) {
            // An escaped UTF-8 lead still contributes to all body flags.
            has_cr |= escaped == b'\r';
            if escaped == b'\n' {
                record_newlines(&mut output, offset, 1);
            }
            if escaped == 0xD8 || escaped == 0xE2 {
                has_bidi |= is_text_flow_control_at(bytes, offset);
            }
            offset += 1;
        }
    }
    StringBodyScan { len: bytes.len(), terminated: false, clean_ascii_no_escape, has_bidi, has_cr }
}

pub fn scan_raw_string_body(bytes: &[u8], hashes: usize) -> Option<(usize, bool, bool)> {
    scan_raw_string_body_inner(bytes, hashes, None)
}

/// Append absolute LF positions while looking for the raw string terminator.
/// On failure, leave `newlines` unchanged for the caller's scalar recovery.
pub fn scan_raw_string_body_with_newlines(
    bytes: &[u8],
    hashes: usize,
    base_offset: u32,
    newlines: &mut Vec<u32>,
) -> Option<(usize, bool, bool)> {
    let nl_start = newlines.len();
    let result = scan_raw_string_body_inner(bytes, hashes, Some((base_offset, &mut *newlines)));
    if result.is_none() {
        // Failure leaves the caller free to collect offsets during scalar recovery.
        newlines.truncate(nl_start);
    }
    result
}

fn scan_raw_string_body_inner(
    bytes: &[u8],
    hashes: usize,
    mut output: Option<(u32, &mut Vec<u32>)>,
) -> Option<(usize, bool, bool)> {
    if hashes > 255 {
        return None;
    }
    let mut offset = 0;
    let mut has_bidi = false;
    let mut has_cr = false;
    while offset < bytes.len() {
        let (masks, len) = masks_at(bytes, offset);
        let mut quotes = masks.quote;
        while quotes != 0 {
            let lane = quotes.trailing_zeros() as usize;
            let after_quote = offset + lane + 1;
            if hashes <= bytes.len() - after_quote
                && bytes[after_quote..after_quote + hashes].iter().all(|&byte| byte == b'#')
            {
                let active = low_bits_mask(lane + 1);
                has_bidi |= bidi_in_mask(bytes, offset, masks.bidi_lead & active);
                has_cr |= masks.cr & active != 0;
                record_newlines(&mut output, offset, masks.newline & active);
                return Some((after_quote + hashes, has_bidi, has_cr));
            }
            quotes &= quotes - 1;
        }
        has_bidi |= bidi_in_mask(bytes, offset, masks.bidi_lead);
        has_cr |= masks.cr != 0;
        record_newlines(&mut output, offset, masks.newline);
        offset += len;
    }
    None
}
