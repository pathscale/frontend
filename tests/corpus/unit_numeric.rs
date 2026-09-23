// Corpus unit: numeric. Free functions only: constants, recursion, `while` loops, `else if`
// chains, a `match` over integer ranges, `u32` to `u64` casts, and calls between them. `Q0` is
// the per-unit tag; see `unit_geometry.rs`.

pub const LIMIT_Q0: u32 = 40;
pub const SCALE_Q0: u64 = 3;

pub fn fib_Q0(n: u32) -> u32 {
    if n < 2 { n } else { fib_Q0(n - 1) + fib_Q0(n - 2) }
}

pub fn tri_Q0(n: u32) -> u64 {
    let mut total: u64 = 0;
    let mut i: u32 = 1;
    while i <= n {
        total = total + (i as u64) * SCALE_Q0;
        i = i + 1;
    }
    total
}

pub fn clamp_Q0(value: u32, low: u32, high: u32) -> u32 {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

pub fn bucket_Q0(value: u32) -> u32 {
    match value {
        0 => 0,
        1..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        _ => 4,
    }
}

pub fn gcd_Q0(a: u32, b: u32) -> u32 {
    let mut x = a;
    let mut y = b;
    while x != y && x != 0 && y != 0 {
        if x > y {
            x = x - y;
        } else {
            y = y - x;
        }
    }
    if x == 0 { y } else { x }
}

pub fn mix_Q0(seed: u32) -> u64 {
    let a = clamp_Q0(seed, 3, LIMIT_Q0);
    let b = bucket_Q0(a * 7);
    let c = gcd_Q0(a + 12, b + 18);
    let f = fib_Q0(clamp_Q0(c, 1, 12)) as u64;
    tri_Q0(b) + f * SCALE_Q0
}
