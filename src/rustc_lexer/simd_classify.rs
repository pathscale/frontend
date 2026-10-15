//! Byte-class masks for complete 64-byte blocks and bounded tails.
//!
//! Bit i always describes byte i, including on big-endian hosts. The masks
//! describe ASCII classes; non-ASCII identifier and bidi validation belongs to
//! the caller.

pub const BLOCK_LEN: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockMasks {
    pub whitespace: u64,
    pub newline: u64,
    pub cr: u64,
    pub quote: u64,
    pub apos: u64,
    pub backslash: u64,
    pub slash: u64,
    pub star: u64,
    pub ident_start: u64,
    pub ident_cont: u64,
    pub digit: u64,
    pub open_brace: u64,
    pub close_brace: u64,
    pub open_paren: u64,
    pub close_paren: u64,
    pub open_bracket: u64,
    pub close_bracket: u64,
    pub semi: u64,
    pub comma: u64,
    pub non_ascii: u64,
    pub bidi_lead: u64,
}

impl BlockMasks {
    #[inline]
    fn masked(mut self, active: u64) -> Self {
        self.whitespace &= active;
        self.newline &= active;
        self.cr &= active;
        self.quote &= active;
        self.apos &= active;
        self.backslash &= active;
        self.slash &= active;
        self.star &= active;
        self.ident_start &= active;
        self.ident_cont &= active;
        self.digit &= active;
        self.open_brace &= active;
        self.close_brace &= active;
        self.open_paren &= active;
        self.close_paren &= active;
        self.open_bracket &= active;
        self.close_bracket &= active;
        self.semi &= active;
        self.comma &= active;
        self.non_ascii &= active;
        self.bidi_lead &= active;
        self
    }
}

#[inline]
pub const fn low_bits_mask(len: usize) -> u64 {
    if len >= BLOCK_LEN { u64::MAX } else { (1_u64 << len) - 1 }
}

// Each backend owns a copy of the input. These safe methods return masks with
// the same lane numbering and never retain pointers into the caller's block.
trait ByteBlock {
    fn eq(&self, byte: u8) -> u64;
    // Only inclusive ASCII ranges with lo <= hi <= 0x7f are used below.
    fn ascii_range(&self, lo: u8, hi: u8) -> u64;
    fn non_ascii(&self) -> u64;
}

#[cfg(not(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_feature = "avx2"),
)))]
use self::SwarBlock as NativeBlock;
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
use self::avx2::Block as NativeBlock;
#[cfg(target_arch = "aarch64")]
use self::neon::Block as NativeBlock;

#[inline]
fn whitespace(block: &impl ByteBlock) -> u64 {
    // ASCII Pattern_White_Space includes vertical tab and form feed.
    block.ascii_range(b'\t', b'\r') | block.eq(b' ')
}

#[inline]
fn ident_start(block: &impl ByteBlock) -> u64 {
    block.ascii_range(b'a', b'z') | block.ascii_range(b'A', b'Z') | block.eq(b'_')
}

#[inline]
fn bidi_lead(block: &impl ByteBlock) -> u64 {
    block.eq(0xe2) | block.eq(0xd8)
}

#[inline]
fn classify(block: &impl ByteBlock) -> BlockMasks {
    let digit = block.ascii_range(b'0', b'9');
    let ident_start = ident_start(block);
    BlockMasks {
        whitespace: whitespace(block),
        newline: block.eq(b'\n'),
        cr: block.eq(b'\r'),
        quote: block.eq(b'"'),
        apos: block.eq(b'\''),
        backslash: block.eq(b'\\'),
        slash: block.eq(b'/'),
        star: block.eq(b'*'),
        ident_start,
        ident_cont: ident_start | digit,
        digit,
        open_brace: block.eq(b'{'),
        close_brace: block.eq(b'}'),
        open_paren: block.eq(b'('),
        close_paren: block.eq(b')'),
        open_bracket: block.eq(b'['),
        close_bracket: block.eq(b']'),
        semi: block.eq(b';'),
        comma: block.eq(b','),
        non_ascii: block.non_ascii(),
        bidi_lead: bidi_lead(block),
    }
}

/// Uses NEON on AArch64, AVX2 when enabled on x86_64, and SWAR otherwise.
#[inline]
pub fn classify_block(block: &[u8; 64]) -> BlockMasks {
    classify(&NativeBlock::new(block))
}

/// Portable eight-byte-at-a-time classification, independent of SIMD features.
#[inline]
pub fn classify_block_swar(block: &[u8; 64]) -> BlockMasks {
    classify(&SwarBlock::new(block))
}

/// Classifies 0..=64 bytes and clears every bit beyond the supplied slice.
///
/// Panics if more than 64 bytes are supplied.
#[inline]
pub fn classify_partial(bytes: &[u8]) -> BlockMasks {
    assert!(bytes.len() <= BLOCK_LEN, "classifier tail exceeds 64 bytes");
    let mut block = [0; BLOCK_LEN];
    block[..bytes.len()].copy_from_slice(bytes);
    classify_block(&block).masked(low_bits_mask(bytes.len()))
}

/// Returns (ASCII Pattern_White_Space, non-ASCII).
#[inline]
pub fn mask_ascii_whitespace_64(block: &[u8; 64]) -> (u64, u64) {
    let block = NativeBlock::new(block);
    (whitespace(&block), block.non_ascii())
}

/// Returns (LF, bidi lead candidates).
#[inline]
pub fn mask_newline_and_bidi_64(block: &[u8; 64]) -> (u64, u64) {
    let block = NativeBlock::new(block);
    (block.eq(b'\n'), bidi_lead(&block))
}

/// Returns (double quote, backslash, CR, non-ASCII).
#[inline]
pub fn mask_string_body_64(block: &[u8; 64]) -> (u64, u64, u64, u64) {
    let block = NativeBlock::new(block);
    (block.eq(b'"'), block.eq(b'\\'), block.eq(b'\r'), block.non_ascii())
}

/// Returns (ASCII identifier continuation, non-ASCII).
#[inline]
pub fn mask_ident_cont_64(block: &[u8; 64]) -> (u64, u64) {
    let block = NativeBlock::new(block);
    (ident_start(&block) | block.ascii_range(b'0', b'9'), block.non_ascii())
}

#[inline]
pub fn mask_digits_and_underscore_64(block: &[u8; 64]) -> u64 {
    let block = NativeBlock::new(block);
    block.ascii_range(b'0', b'9') | block.eq(b'_')
}

const BYTE_ONES: u64 = 0x0101_0101_0101_0101;
const BYTE_HIGH: u64 = 0x8080_8080_8080_8080;
const BYTE_LOW: u64 = 0x7f7f_7f7f_7f7f_7f7f;

struct SwarBlock([u64; 8]);

impl SwarBlock {
    #[inline]
    fn new(block: &[u8; 64]) -> Self {
        // Explicit little-endian assembly fixes lane order on every host.
        Self(core::array::from_fn(|word| {
            u64::from_le_bytes(core::array::from_fn(|lane| block[word * 8 + lane]))
        }))
    }

    #[inline]
    fn collect(&self, predicate: impl Fn(u64) -> u64) -> u64 {
        let mut mask = 0;
        for (index, &word) in self.0.iter().enumerate() {
            let high = predicate(word) & BYTE_HIGH;
            // After shifting, each byte is 0 or 1. Multiplication assigns
            // successive bytes weights 1, 2, ..., 128 in the top byte.
            let packed = (high >> 7).wrapping_mul(0x0102_0408_1020_4080) >> 56;
            mask |= packed << (index * 8);
        }
        mask
    }
}

impl ByteBlock for SwarBlock {
    #[inline]
    fn eq(&self, byte: u8) -> u64 {
        let repeated = u64::from(byte) * BYTE_ONES;
        self.collect(|word| {
            let different = word ^ repeated;
            // Low-seven-bit addition cannot carry between bytes. Unlike
            // subtract-one zero detection, this marks each lane exactly,
            // without a borrow contaminating the next lane.
            !(((different & BYTE_LOW) + BYTE_LOW) | different | BYTE_LOW)
        })
    }

    #[inline]
    fn ascii_range(&self, lo: u8, hi: u8) -> u64 {
        debug_assert!(lo <= hi && hi <= 0x7f);
        let lower_bias = u64::from(0x80 - lo) * BYTE_ONES;
        let upper_bias = u64::from(0x80 + hi) * BYTE_ONES;
        self.collect(|word| {
            let low = word & BYTE_LOW;
            // Each addition is at most 255; each subtraction is nonnegative
            // within its byte. Neither propagates between lanes. Original
            // high bits exclude non-ASCII bytes from both bounds.
            (low + lower_bias) & (upper_bias - low) & !word
        })
    }

    #[inline]
    fn non_ascii(&self) -> u64 {
        self.collect(|word| word)
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
mod avx2 {
    use core::arch::x86_64::*;

    use super::ByteBlock;

    pub(super) struct Block([__m256i; 2]);

    impl Block {
        #[inline]
        pub(super) fn new(block: &[u8; 64]) -> Self {
            // SAFETY: This module requires AVX2 at compile time. loadu accepts
            // byte alignment; offsets 0 and 32 each read 32 initialized bytes
            // inside this 64-byte shared array. Reads do not mutate or alias
            // any mutable reference, and the resulting vectors own their data.
            unsafe {
                Self([
                    _mm256_loadu_si256(block.as_ptr().cast()),
                    _mm256_loadu_si256(block.as_ptr().add(32).cast()),
                ])
            }
        }

        #[inline]
        fn collect(&self, predicate: impl Fn(__m256i) -> __m256i) -> u64 {
            // SAFETY: AVX2 is enabled for this module. Both inputs and both
            // predicate results are initialized, owned vector values. These
            // register operations access no pointers and have no memory
            // alignment, bounds or aliasing requirements.
            unsafe {
                u64::from(_mm256_movemask_epi8(predicate(self.0[0])) as u32)
                    | (u64::from(_mm256_movemask_epi8(predicate(self.0[1])) as u32) << 32)
            }
        }
    }

    impl ByteBlock for Block {
        #[inline]
        fn eq(&self, byte: u8) -> u64 {
            self.collect(|word| {
                // SAFETY: AVX2 is enabled; initialized vector values are
                // compared entirely in registers, with no pointer accesses
                // or alignment, bounds or aliasing requirements.
                unsafe { _mm256_cmpeq_epi8(word, _mm256_set1_epi8(byte as i8)) }
            })
        }

        #[inline]
        fn ascii_range(&self, lo: u8, hi: u8) -> u64 {
            debug_assert!(lo <= hi && hi <= 0x7f);
            self.collect(|word| {
                // SAFETY: AVX2 is enabled. Initialized vectors are used only
                // in register operations; there are no pointer, alignment,
                // bounds or aliasing requirements. Signed comparison rejects
                // high-bit bytes as negative at the inclusive lower bound.
                unsafe {
                    let above = _mm256_cmpgt_epi8(word, _mm256_set1_epi8(lo as i8 - 1));
                    // Invert word > hi rather than computing hi + 1, which
                    // would overflow a signed byte for the ASCII upper bound.
                    let beyond = _mm256_cmpgt_epi8(word, _mm256_set1_epi8(hi as i8));
                    _mm256_andnot_si256(beyond, above)
                }
            })
        }

        #[inline]
        fn non_ascii(&self) -> u64 {
            self.collect(|word| word)
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use core::arch::aarch64::*;

    use super::ByteBlock;

    pub(super) struct Block([uint8x16_t; 4]);

    impl Block {
        #[inline]
        pub(super) fn new(block: &[u8; 64]) -> Self {
            // SAFETY: NEON is available on the supported AArch64 target.
            // vld1q_u8 accepts byte alignment. Offsets 0, 16, 32 and 48 each
            // read 16 initialized bytes within the shared 64-byte array.
            // The loads are read-only and the returned vectors own their data,
            // so no mutable aliases or borrowed pointers escape.
            unsafe {
                Self([
                    vld1q_u8(block.as_ptr()),
                    vld1q_u8(block.as_ptr().add(16)),
                    vld1q_u8(block.as_ptr().add(32)),
                    vld1q_u8(block.as_ptr().add(48)),
                ])
            }
        }

        #[inline]
        fn collect(&self, predicate: impl Fn(uint8x16_t) -> uint8x16_t) -> u64 {
            let weights = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
            // SAFETY: NEON is available. The byte-aligned weights array has
            // exactly 16 initialized bytes and is only read. All remaining
            // operations use initialized owned vectors without pointer
            // accesses or aliasing. Each eight-lane sum is at most 255.
            unsafe {
                let weights = vld1q_u8(weights.as_ptr());
                let mut mask = 0;
                for (index, &word) in self.0.iter().enumerate() {
                    // All NEON predicates return 0xff for matching lanes
                    // and zero otherwise. Each selected lane adds its weight.
                    let selected = vandq_u8(predicate(word), weights);
                    let low = u64::from(vaddv_u8(vget_low_u8(selected)));
                    let high = u64::from(vaddv_u8(vget_high_u8(selected)));
                    mask |= (low | (high << 8)) << (index * 16);
                }
                mask
            }
        }
    }

    impl ByteBlock for Block {
        #[inline]
        fn eq(&self, byte: u8) -> u64 {
            self.collect(|word| {
                // SAFETY: NEON is available. These operations consume
                // initialized vectors only and access no pointers, so memory
                // alignment, bounds and aliasing requirements do not arise.
                unsafe { vceqq_u8(word, vdupq_n_u8(byte)) }
            })
        }

        #[inline]
        fn ascii_range(&self, lo: u8, hi: u8) -> u64 {
            debug_assert!(lo <= hi && hi <= 0x7f);
            self.collect(|word| {
                // SAFETY: NEON is available. All values are initialized;
                // these unsigned comparisons and the AND operate solely in
                // registers with no pointer, alignment, bounds or aliasing
                // requirements. The upper bound excludes non-ASCII bytes.
                unsafe { vandq_u8(vcgeq_u8(word, vdupq_n_u8(lo)), vcleq_u8(word, vdupq_n_u8(hi))) }
            })
        }

        #[inline]
        fn non_ascii(&self) -> u64 {
            self.collect(|word| {
                // SAFETY: NEON is available. Initialized vectors are compared
                // entirely in registers without pointer accesses or memory
                // alignment, bounds or aliasing requirements.
                unsafe { vcgeq_u8(word, vdupq_n_u8(0x80)) }
            })
        }
    }
}
