//! ASCII identifier, lifetime and digit scanners with scanner-relative lengths.

use core::convert::TryInto;

use super::simd_classify::{BLOCK_LEN, mask_digits_and_underscore_64, mask_ident_cont_64};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentScan {
    AsciiOnly(usize),
    NeedsUnicodeAt(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifetimeScan {
    Lifetime { len: usize, starts_with_number: bool },
    CharOrOther,
}

/// Scan an identifier continuation, excluding any byte consumed by the caller.
pub fn scan_ascii_ident_tail(bytes: &[u8]) -> IdentScan {
    let mut len = 0;
    while bytes.len() - len >= BLOCK_LEN {
        let block = bytes[len..len + BLOCK_LEN].try_into().unwrap();
        let (ident_cont, non_ascii) = mask_ident_cont_64(block);
        if ident_cont != u64::MAX {
            let stop = (!ident_cont).trailing_zeros() as usize;
            return if non_ascii & (1u64 << stop) != 0 {
                IdentScan::NeedsUnicodeAt(len + stop)
            } else {
                IdentScan::AsciiOnly(len + stop)
            };
        }
        len += BLOCK_LEN;
    }
    while let Some(&byte) = bytes.get(len) {
        if !is_ident_cont(byte) {
            return if byte.is_ascii() {
                IdentScan::AsciiOnly(len)
            } else {
                IdentScan::NeedsUnicodeAt(len)
            };
        }
        len += 1;
    }
    IdentScan::AsciiOnly(len)
}

/// Scan an identifier including its first byte, if that byte is an ASCII start.
pub fn scan_ascii_ident_full(bytes: &[u8]) -> Option<IdentScan> {
    if !is_ident_start(*bytes.first()?) {
        return None;
    }
    Some(match scan_ascii_ident_tail(&bytes[1..]) {
        IdentScan::AsciiOnly(len) => IdentScan::AsciiOnly(len + 1),
        IdentScan::NeedsUnicodeAt(len) => IdentScan::NeedsUnicodeAt(len + 1),
    })
}

/// Input and returned length exclude the opening apostrophe.
/// Ambiguous character, raw and unknown-prefix forms require scalar handling.
pub fn scan_ascii_lifetime(bytes: &[u8]) -> LifetimeScan {
    let Some(&first) = bytes.first() else {
        return LifetimeScan::CharOrOther;
    };
    let starts_with_number = first.is_ascii_digit();
    if !is_ident_start(first) && !starts_with_number {
        return LifetimeScan::CharOrOther;
    }
    let len = match scan_ascii_ident_tail(&bytes[1..]) {
        IdentScan::AsciiOnly(len) => len + 1,
        IdentScan::NeedsUnicodeAt(_) => return LifetimeScan::CharOrOther,
    };
    if matches!(bytes.get(len), Some(b'\'' | b'"' | b'#')) {
        return LifetimeScan::CharOrOther;
    }
    LifetimeScan::Lifetime { len, starts_with_number }
}

/// Consume decimal digits and underscores, reporting whether a digit occurred.
pub fn scan_decimal_digits(bytes: &[u8]) -> (usize, bool) {
    let mut len = 0;
    let mut has_digit = false;
    while bytes.len() - len >= BLOCK_LEN {
        let block = bytes[len..len + BLOCK_LEN].try_into().unwrap();
        let accepted = mask_digits_and_underscore_64(block);
        let consumed = (!accepted).trailing_zeros() as usize;
        has_digit = has_digit || block[..consumed].iter().any(|&byte| byte != b'_');
        len += consumed;
        if consumed != BLOCK_LEN {
            return (len, has_digit);
        }
    }
    let (tail_len, tail_has_digit) = scan_digits(&bytes[len..], b'9', false);
    (len + tail_len, has_digit || tail_has_digit)
}

/// Consume hexadecimal digits and underscores.
pub fn scan_hex_digits(bytes: &[u8]) -> (usize, bool) {
    scan_digits(bytes, b'9', true)
}

/// Consume only valid octal digits and underscores, leaving `8` and `9` unread.
pub fn scan_octal_digits(bytes: &[u8]) -> (usize, bool) {
    scan_digits(bytes, b'7', false)
}

/// Consume only valid binary digits and underscores, leaving `2`..`9` unread.
pub fn scan_binary_digits(bytes: &[u8]) -> (usize, bool) {
    scan_digits(bytes, b'1', false)
}

#[inline]
fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

#[inline]
fn is_ident_cont(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

const BYTE_HIGH_BITS: u64 = 0x8080_8080_8080_8080;
const BYTE_LOW_BITS: u64 = 0x7f7f_7f7f_7f7f_7f7f;
const BYTE_ONES: u64 = 0x0101_0101_0101_0101;

/// Set each lane's high bit exactly when its byte is in the ASCII range.
#[inline]
fn ascii_range_mask(word: u64, lo: u8, hi: u8) -> u64 {
    // Clearing high bits keeps both additions within each byte: their largest
    // sum is 127 + 128, so no carry can affect a neighboring lane. Excluding
    // original high bits ensures that non-ASCII bytes never match a range.
    let low = word & BYTE_LOW_BITS;
    let at_least_lo = low + BYTE_ONES * u64::from(128 - lo);
    let above_hi = low + BYTE_ONES * u64::from(127 - hi);
    at_least_lo & !above_hi & !word & BYTE_HIGH_BITS
}

fn scan_digits(bytes: &[u8], last_digit: u8, hex: bool) -> (usize, bool) {
    let mut len = 0;
    let mut has_digit = false;
    while bytes.len() - len >= 8 {
        // A value load from an exactly bounded array needs neither alignment
        // assumptions nor pointer casts. Little endian fixes lane order on all
        // targets, including big-endian targets.
        let word = u64::from_le_bytes(bytes[len..len + 8].try_into().unwrap());
        let mut digits = ascii_range_mask(word, b'0', last_digit);
        if hex {
            digits |= ascii_range_mask(word, b'a', b'f') | ascii_range_mask(word, b'A', b'F');
        }
        let accepted = digits | ascii_range_mask(word, b'_', b'_');
        let rejected = !accepted & BYTE_HIGH_BITS;
        if rejected == 0 {
            has_digit |= digits != 0;
            len += 8;
        } else {
            let consumed = rejected.trailing_zeros() as usize / 8;
            // A rejected lane exists, so consumed is at most seven and the
            // shift is strictly smaller than the word width, even for zero.
            has_digit |= digits & ((1u64 << (consumed * 8)) - 1) != 0;
            return (len + consumed, has_digit);
        }
    }
    while let Some(&byte) = bytes.get(len) {
        if byte == b'_' {
            len += 1;
        } else if (b'0'..=last_digit).contains(&byte) || (hex && byte.is_ascii_hexdigit()) {
            has_digit = true;
            len += 1;
        } else {
            break;
        }
    }
    (len, has_digit)
}
