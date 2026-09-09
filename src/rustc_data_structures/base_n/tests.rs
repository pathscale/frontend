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

#[test]
fn limits() {
    assert_eq!(Ok(u128::MAX), u128::from_str_radix(&u128::MAX.to_base(36), 36));
    assert_eq!(Ok(u64::MAX), u64::from_str_radix(&u64::MAX.to_base(36), 36));
    assert_eq!(Ok(u32::MAX), u32::from_str_radix(&u32::MAX.to_base(36), 36));
}

#[test]
fn test_to_base() {
    fn test(n: u128, base: usize) {
        assert_eq!(Ok(n), u128::from_str_radix(&n.to_base(base), base as u32));
        assert_eq!(Ok(n), u128::from_str_radix(&n.to_base_fixed_len(base), base as u32));
    }

    for base in 2..37 {
        test(0, base);
        test(1, base);
        test(35, base);
        test(36, base);
        test(37, base);
        test(u64::MAX as u128, base);
        test(u128::MAX, base);

        const N: u128 = if cfg!(miri) { 10 } else { 1000 };

        for i in 0..N {
            test(i * 983, base);
        }
    }
}
