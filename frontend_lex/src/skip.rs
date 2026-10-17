//! The prescan's inner loop: the next byte that can change its state, sixteen bytes at a time.
//!
//! The stop set is `" ' / \n ( ) [ ] { } ;`. x86-64 uses shared compares; AArch64 uses two
//! nibble lookup tables and a test, then narrows predicates to one bit per byte. Elsewhere, and
//! for a tail shorter than a block, a scalar table does it.

/// Bytes the prescan stops at in code.
pub(crate) const STOP: [bool; 256] = {
    let mut table = [false; 256];
    let stops = b"\"'/\n(){}[];";
    let mut i = 0;
    while i < stops.len() {
        table[stops[i] as usize] = true;
        i += 1;
    }
    table
};

/// The first `i` in `pos..end` with `STOP[src[i]]`, or `end`.
#[inline(always)]
pub(crate) fn next_stop(src: &[u8], mut pos: usize, end: usize) -> usize {
    debug_assert!(end <= src.len());
    #[cfg(any(
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_feature = "neon", target_endian = "little")
    ))]
    while pos + 16 <= end {
        // SAFETY: `pos + 16 <= end <= src.len()`, so the block is in bounds.
        if let Some(at) = unsafe { block(src.as_ptr().add(pos)) } {
            return pos + at;
        }
        pos += 16;
    }
    while pos < end && !STOP[src[pos] as usize] {
        pos += 1;
    }
    pos
}

/// The offset of the first stop byte in the 16 bytes at `p`, if any.
///
/// # Safety
///
/// `p` must be valid for reading 16 bytes.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn block(p: *const u8) -> Option<usize> {
    use core::arch::x86_64::*;
    // SAFETY: the caller guarantees 16 readable bytes; SSE2 is baseline on x86-64.
    unsafe {
        let v = _mm_loadu_si128(p.cast());
        let eq = |b: u8| _mm_cmpeq_epi8(v, _mm_set1_epi8(b as i8));
        let paren =
            _mm_cmpeq_epi8(_mm_and_si128(v, _mm_set1_epi8(!1u8 as i8)), _mm_set1_epi8(0x28));
        let folded = _mm_and_si128(v, _mm_set1_epi8(!0x20u8 as i8));
        let open = _mm_cmpeq_epi8(folded, _mm_set1_epi8(0x5B));
        let close = _mm_cmpeq_epi8(folded, _mm_set1_epi8(0x5D));
        let any = _mm_or_si128(
            _mm_or_si128(_mm_or_si128(eq(b'"'), eq(b'\'')), _mm_or_si128(eq(b'/'), eq(b'\n'))),
            _mm_or_si128(_mm_or_si128(paren, open), _mm_or_si128(close, eq(b';'))),
        );
        let mask = _mm_movemask_epi8(any) as u32;
        if mask == 0 { None } else { Some(mask.trailing_zeros() as usize) }
    }
}

/// The offset of the first stop byte in the 16 bytes at `p`, if any.
///
/// # Safety
///
/// `p` must be valid for reading 16 bytes.
#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
#[inline(always)]
unsafe fn block(p: *const u8) -> Option<usize> {
    // SAFETY: the caller guarantees 16 readable bytes.
    let mask = unsafe { crate::simd::stop_mask(p) };
    if mask == 0 { None } else { Some(mask.trailing_zeros() as usize / 4) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn matches_the_table() {
        let mut src = alloc::vec::Vec::new();
        for i in 0..4096u32 {
            src.push((i.wrapping_mul(2654435761) >> 24) as u8);
        }
        for pos in 0..src.len() {
            let want =
                (pos..src.len()).find(|&i| super::STOP[src[i] as usize]).unwrap_or(src.len());
            assert_eq!(super::next_stop(&src, pos, src.len()), want, "at {pos}");
        }
    }
}
