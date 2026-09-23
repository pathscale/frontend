//! Converts unsigned integers into a string representation with some base.
//! Bases up to and including 36 can be used for case-insensitive things.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::fmt;

#[cfg(test)]
mod tests;

pub const MAX_BASE: usize = 64;
pub const ALPHANUMERIC_ONLY: usize = 62;
pub const CASE_INSENSITIVE: usize = 36;

// Plain bytes where upstream has `ascii::Char`, which is the unstable `ascii_char`. The type was
// only there to carry the proof that the buffer is ASCII; the `const` block below checks it
// instead, and `as_str` relies on it.
const BASE_64: [u8; MAX_BASE] = {
    let bytes = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ@$";
    assert!(bytes.is_ascii());
    *bytes
};

pub struct BaseNString {
    start: usize,
    buf: [u8; 128],
}

impl BaseNString {
    fn as_str(&self) -> &str {
        let digits = &self.buf[self.start..];
        // SAFETY: every byte in `buf` is `b'0'` or was copied out of `BASE_64`, which is
        // checked to be ASCII at compile time, and ASCII is valid UTF-8.
        unsafe { core::str::from_utf8_unchecked(digits) }
    }
}

impl core::ops::Deref for BaseNString {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for BaseNString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for BaseNString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self)
    }
}

// This trait just lets us reserve the exact right amount of space when doing fixed-length
// case-insensitive encoding. Add any impls you need.
pub trait ToBaseN: Into<u128> {
    fn encoded_len(base: usize) -> usize;

    fn to_base_fixed_len(self, base: usize) -> BaseNString {
        let mut encoded = self.to_base(base);
        encoded.start = encoded.buf.len() - Self::encoded_len(base);
        encoded
    }

    fn to_base(self, base: usize) -> BaseNString {
        let mut output = [b'0'; 128];

        let mut n: u128 = self.into();

        let mut index = output.len();
        loop {
            index -= 1;
            output[index] = BASE_64[(n % base as u128) as usize];
            n /= base as u128;

            if n == 0 {
                break;
            }
        }
        assert_eq!(n, 0);

        BaseNString { start: index, buf: output }
    }
}

impl ToBaseN for u128 {
    fn encoded_len(base: usize) -> usize {
        let mut max = u128::MAX;
        let mut len = 0;
        while max > 0 {
            len += 1;
            max /= base as u128;
        }
        len
    }
}

impl ToBaseN for u64 {
    fn encoded_len(base: usize) -> usize {
        let mut max = u64::MAX;
        let mut len = 0;
        while max > 0 {
            len += 1;
            max /= base as u64;
        }
        len
    }
}

impl ToBaseN for u32 {
    fn encoded_len(base: usize) -> usize {
        let mut max = u32::MAX;
        let mut len = 0;
        while max > 0 {
            len += 1;
            max /= base as u32;
        }
        len
    }
}
