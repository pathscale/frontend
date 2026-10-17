//! Small bounded AArch64 NEON kernels used by the lexer.

#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
const STOP_LO: [u8; 16] = [0, 0, 2, 0, 0, 0, 0, 2, 2, 2, 1, 12, 0, 8, 0, 2];
#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
const STOP_HI: [u8; 16] = [1, 0, 2, 4, 0, 8, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0];
#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
const IDENT_LO: [u8; 16] = [5, 7, 7, 7, 7, 7, 7, 7, 7, 7, 6, 2, 2, 2, 2, 10];
#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
const IDENT_HI: [u8; 16] = [0, 0, 0, 1, 2, 12, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0];
#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
const WS_TABLE: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 32, 32, 32, 32, 0, 0];

#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
#[inline(always)]
pub(crate) unsafe fn stop_mask(p: *const u8) -> u64 {
    use core::arch::aarch64::*;
    unsafe {
        let v = vld1q_u8(p);
        let lo_lut = vld1q_u8(STOP_LO.as_ptr());
        let hi_lut = vld1q_u8(STOP_HI.as_ptr());
        let lo = vandq_u8(v, vdupq_n_u8(15));
        let hi = vshrq_n_u8::<4>(v);
        let hit = vtstq_u8(vqtbl1q_u8(lo_lut, lo), vqtbl1q_u8(hi_lut, hi));
        vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(hit))))
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
#[inline(always)]
pub(crate) unsafe fn ident_reject_mask(p: *const u8) -> u64 {
    use core::arch::aarch64::*;
    unsafe {
        let v = vld1q_u8(p);
        let lo_lut = vld1q_u8(IDENT_LO.as_ptr());
        let hi_lut = vld1q_u8(IDENT_HI.as_ptr());
        let lo = vandq_u8(v, vdupq_n_u8(15));
        let hi = vshrq_n_u8::<4>(v);
        let accepted = vtstq_u8(vqtbl1q_u8(lo_lut, lo), vqtbl1q_u8(hi_lut, hi));
        let rejected = vmvnq_u8(accepted);
        vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(rejected))))
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
#[inline(always)]
pub(crate) unsafe fn whitespace_masks(p: *const u8) -> (u64, u64) {
    use core::arch::aarch64::*;
    unsafe {
        let v = vld1q_u8(p);
        let t = vld1q_u8(WS_TABLE.as_ptr());
        let matched = vceqq_u8(vqtbx1q_u8(v, t, v), vdupq_n_u8(32));
        let lf = vceqq_u8(v, vdupq_n_u8(10));
        let pack =
            |x| vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(x))));
        (pack(matched), pack(lf))
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
#[inline(always)]
pub(crate) unsafe fn segment_masks(p: *const u8) -> (u64, u64, u64) {
    use core::arch::aarch64::*;
    unsafe {
        let v = vld1q_u8(p);
        let lf = vceqq_u8(v, vdupq_n_u8(10));
        let cr = vceqq_u8(v, vdupq_n_u8(13));
        let non_ascii = vcgeq_u8(v, vdupq_n_u8(0xc0));
        let pack =
            |x| vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(x))));
        (pack(lf), pack(cr), pack(non_ascii))
    }
}

#[cfg(all(test, target_arch = "aarch64", target_feature = "neon", target_endian = "little"))]
mod tests {
    #[test]
    fn kernels_match_scalar_bytes_in_every_lane() {
        const LANES: u64 = 0x8888_8888_8888_8888;
        for byte in 0..=u8::MAX {
            for lane in 0..16 {
                let mut bytes = [0; 16];
                bytes[lane] = byte;
                // SAFETY: the array provides a complete readable block for every kernel.
                let (stop, reject, (space, newline), (lf, cr, lead)) = unsafe {
                    (
                        super::stop_mask(bytes.as_ptr()),
                        super::ident_reject_mask(bytes.as_ptr()),
                        super::whitespace_masks(bytes.as_ptr()),
                        super::segment_masks(bytes.as_ptr()),
                    )
                };
                let mask = |predicate: fn(u8) -> bool| {
                    bytes.iter().enumerate().fold(0, |bits, (i, &b)| {
                        bits | if predicate(b) { 8u64 << (i * 4) } else { 0 }
                    })
                };
                assert_eq!(stop & LANES, mask(|b| crate::skip::STOP[b as usize]));
                assert_eq!(reject & LANES, mask(|b| !(b.is_ascii_alphanumeric() || b == b'_')));
                assert_eq!(
                    space & LANES,
                    mask(|b| matches!(b, b' ' | b'\t' | b'\n' | 11 | 12 | b'\r'))
                );
                assert_eq!(newline & LANES, mask(|b| b == b'\n'));
                assert_eq!(lf & LANES, mask(|b| b == b'\n'));
                assert_eq!(cr & LANES, mask(|b| b == b'\r'));
                assert_eq!(lead & LANES, mask(|b| b >= 0xc0));
            }
        }
    }
}
