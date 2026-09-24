// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::*;

#[cfg(test)]
mod tests;

/// Finds all newlines, multi-byte characters, and non-narrow characters in a
/// SourceFile.
///
/// This function uses a NEON implementation on aarch64, and an SSE2 one on x86 when
/// hardware support is detected at runtime.
pub(crate) fn analyze_source_file(src: &str) -> (Vec<RelativeBytePos>, Vec<MultiByteChar>) {
    // Rust source averages well over 20 bytes a line, so one entry per 40 bytes covers most
    // of a file without regrowth; capacity never changes what is pushed.
    let mut lines = Vec::with_capacity(src.len() / 40 + 1);
    lines.push(RelativeBytePos::from_u32(0));
    let mut multi_byte_chars = vec![];

    // Calls the right implementation, depending on hardware support available.
    analyze_source_file_dispatch(src, &mut lines, &mut multi_byte_chars);

    // The code above optimistically registers a new line *after* each \n
    // it encounters. If that point is already outside the source_file, remove
    // it again.
    if let Some(&last_line_start) = lines.last() {
        let source_file_end = RelativeBytePos::from_usize(src.len());
        assert!(source_file_end >= last_line_start);
        if last_line_start == source_file_end {
            lines.pop();
        }
    }

    (lines, multi_byte_chars)
}

cfg_select! {
    any(target_arch = "x86", target_arch = "x86_64") => {
        fn analyze_source_file_dispatch(
            src: &str,
            lines: &mut Vec<RelativeBytePos>,
            multi_byte_chars: &mut Vec<MultiByteChar>,
        ) {
            if is_x86_feature_detected!("sse2") {
                unsafe {
                    analyze_source_file_sse2(src, lines, multi_byte_chars);
                }
            } else {
                analyze_source_file_generic(
                    src,
                    src.len(),
                    RelativeBytePos::from_u32(0),
                    lines,
                    multi_byte_chars,
                );
            }
        }

        /// Checks 16 byte chunks of text at a time. If the chunk contains
        /// something other than printable ASCII characters and newlines, the
        /// function falls back to the generic implementation. Otherwise it uses
        /// SSE2 intrinsics to quickly find all newlines.
        #[target_feature(enable = "sse2")]
        unsafe fn analyze_source_file_sse2(
            src: &str,
            lines: &mut Vec<RelativeBytePos>,
            multi_byte_chars: &mut Vec<MultiByteChar>,
        ) {
            #[cfg(target_arch = "x86")]
            use core::arch::x86::*;
            #[cfg(target_arch = "x86_64")]
            use core::arch::x86_64::*;

            const CHUNK_SIZE: usize = 16;

            let (chunks, tail) = src.as_bytes().as_chunks::<CHUNK_SIZE>();

            // This variable keeps track of where we should start decoding a
            // chunk. If a multi-byte character spans across chunk boundaries,
            // we need to skip that part in the next chunk because we already
            // handled it.
            let mut intra_chunk_offset = 0;

            for (chunk_index, chunk) in chunks.iter().enumerate() {
                // We don't know if the pointer is aligned to 16 bytes, so we
                // use `loadu`, which supports unaligned loading.
                let chunk = unsafe { _mm_loadu_si128(chunk.as_ptr() as *const __m128i) };

                // For each character in the chunk, see if its byte value is < 0,
                // which indicates that it's part of a UTF-8 char.
                let multibyte_test = _mm_cmplt_epi8(chunk, _mm_set1_epi8(0));
                // Create a bit mask from the comparison results.
                let multibyte_mask = _mm_movemask_epi8(multibyte_test);

                // If the bit mask is all zero, we only have ASCII chars here:
                if multibyte_mask == 0 {
                    assert!(intra_chunk_offset == 0);

                    // Check for newlines in the chunk
                    let newlines_test = _mm_cmpeq_epi8(chunk, _mm_set1_epi8(b'\n' as i8));
                    let mut newlines_mask = _mm_movemask_epi8(newlines_test);

                    let output_offset = RelativeBytePos::from_usize(chunk_index * CHUNK_SIZE + 1);

                    while newlines_mask != 0 {
                        let index = newlines_mask.trailing_zeros();

                        lines.push(RelativeBytePos(index) + output_offset);

                        // Clear the bit, so we can find the next one.
                        newlines_mask &= newlines_mask - 1;
                    }
                } else {
                    // The slow path.
                    // There are multibyte chars in here, fallback to generic decoding.
                    let scan_start = chunk_index * CHUNK_SIZE + intra_chunk_offset;
                    intra_chunk_offset = analyze_source_file_generic(
                        &src[scan_start..],
                        CHUNK_SIZE - intra_chunk_offset,
                        RelativeBytePos::from_usize(scan_start),
                        lines,
                        multi_byte_chars,
                    );
                }
            }

            // There might still be a tail left to analyze
            let tail_start = src.len() - tail.len() + intra_chunk_offset;
            if tail_start < src.len() {
                analyze_source_file_generic(
                    &src[tail_start..],
                    src.len() - tail_start,
                    RelativeBytePos::from_usize(tail_start),
                    lines,
                    multi_byte_chars,
                );
            }
        }
    }
    all(target_arch = "aarch64", target_feature = "neon") => {
        // NEON is part of the aarch64 baseline on every target this arm compiles for (the
        // `target_feature` test above is static), so there is no runtime detection.
        fn analyze_source_file_dispatch(
            src: &str,
            lines: &mut Vec<RelativeBytePos>,
            multi_byte_chars: &mut Vec<MultiByteChar>,
        ) {
            analyze_source_file_neon(src, lines, multi_byte_chars);
        }

        /// The SSE2 path above, on NEON: 16 byte chunks, an all-ASCII chunk has its
        /// newlines found from a comparison mask, and a chunk holding any byte >= 0x80
        /// falls back to the generic decoder, which also reports how far a multi-byte
        /// character runs into the next chunk.
        fn analyze_source_file_neon(
            src: &str,
            lines: &mut Vec<RelativeBytePos>,
            multi_byte_chars: &mut Vec<MultiByteChar>,
        ) {
            use core::arch::aarch64::*;

            const CHUNK_SIZE: usize = 16;

            let (chunks, tail) = src.as_bytes().as_chunks::<CHUNK_SIZE>();

            // Where decoding of the current chunk starts: a multi-byte character that
            // crossed the previous chunk boundary has already been handled.
            let mut intra_chunk_offset = 0;

            let newline = vdupq_n_u8(b'\n');

            for (chunk_index, chunk) in chunks.iter().enumerate() {
                // SAFETY: `chunk` is exactly 16 readable bytes; `vld1q_u8` has no
                // alignment requirement.
                let chunk = unsafe { vld1q_u8(chunk.as_ptr()) };

                // The largest byte is below 0x80 exactly when the chunk is all ASCII.
                if vmaxvq_u8(chunk) < 0x80 {
                    // A character crossing into this chunk would leave a continuation
                    // byte (>= 0x80) here.
                    assert!(intra_chunk_offset == 0);

                    // 0xFF per newline byte, narrowed to one nibble per byte: nibble `i`
                    // of the u64 is byte `i` of the chunk. Keep one bit per nibble so
                    // `trailing_zeros / 4` is the byte index and `m & (m - 1)` clears it.
                    let newlines_test = vceqq_u8(chunk, newline);
                    let narrowed = vshrn_n_u16::<4>(vreinterpretq_u16_u8(newlines_test));
                    let mut newlines_mask =
                        vget_lane_u64::<0>(vreinterpret_u64_u8(narrowed)) & 0x8888_8888_8888_8888;

                    let output_offset = RelativeBytePos::from_usize(chunk_index * CHUNK_SIZE + 1);

                    while newlines_mask != 0 {
                        let index = newlines_mask.trailing_zeros() / 4;

                        lines.push(RelativeBytePos(index) + output_offset);

                        newlines_mask &= newlines_mask - 1;
                    }
                } else {
                    // The slow path, exactly as in the SSE2 arm.
                    let scan_start = chunk_index * CHUNK_SIZE + intra_chunk_offset;
                    intra_chunk_offset = analyze_source_file_generic(
                        &src[scan_start..],
                        CHUNK_SIZE - intra_chunk_offset,
                        RelativeBytePos::from_usize(scan_start),
                        lines,
                        multi_byte_chars,
                    );
                }
            }

            // There might still be a tail left to analyze.
            let tail_start = src.len() - tail.len() + intra_chunk_offset;
            if tail_start < src.len() {
                analyze_source_file_generic(
                    &src[tail_start..],
                    src.len() - tail_start,
                    RelativeBytePos::from_usize(tail_start),
                    lines,
                    multi_byte_chars,
                );
            }
        }
    }
    // The loongarch64 arm is gone. It was the only place left reaching for `std`:
    // `is_loongarch_feature_detected!` is runtime CPU feature detection, which lives in
    // `std_detect` and has no `core` equivalent - so the LSX path could not be selected without
    // std. This compiler targets aarch64; the scalar fallback below is what it has always run.
    _ => {
        // The target (or compiler version) does not support vector instructions
        // our specialized implementations need (x86 SSE2, aarch64 NEON)...
        fn analyze_source_file_dispatch(
            src: &str,
            lines: &mut Vec<RelativeBytePos>,
            multi_byte_chars: &mut Vec<MultiByteChar>,
        ) {
            analyze_source_file_generic(
                src,
                src.len(),
                RelativeBytePos::from_u32(0),
                lines,
                multi_byte_chars,
            );
        }
    }
}

// `scan_len` determines the number of bytes in `src` to scan. Note that the
// function can read past `scan_len` if a multi-byte character start within the
// range but extends past it. The overflow is returned by the function.
fn analyze_source_file_generic(
    src: &str,
    scan_len: usize,
    output_offset: RelativeBytePos,
    lines: &mut Vec<RelativeBytePos>,
    multi_byte_chars: &mut Vec<MultiByteChar>,
) -> usize {
    assert!(src.len() >= scan_len);
    let mut i = 0;
    let src_bytes = src.as_bytes();

    while i < scan_len {
        let byte = unsafe {
            // We verified that i < scan_len <= src.len()
            *src_bytes.get_unchecked(i)
        };

        // How much to advance in order to get to the next UTF-8 char in the
        // string.
        let mut char_len = 1;

        if byte == b'\n' {
            let pos = RelativeBytePos::from_usize(i) + output_offset;
            lines.push(pos + RelativeBytePos(1));
        } else if byte >= 128 {
            // This is the beginning of a multibyte char. Just decode to `char`.
            let c = src[i..].chars().next().unwrap();
            char_len = c.len_utf8();

            let pos = RelativeBytePos::from_usize(i) + output_offset;
            assert!((2..=4).contains(&char_len));
            let mbc = MultiByteChar { pos, bytes: char_len as u8 };
            multi_byte_chars.push(mbc);
        }

        i += char_len;
    }

    i - scan_len
}
