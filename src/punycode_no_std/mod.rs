//! Punycode encoding, [RFC 3492](https://www.rfc-editor.org/rfc/rfc3492).
//!
//! The `v0` mangling scheme spells a non-ASCII identifier as `u<len><punycode>`, so this is
//! on the path that decides symbol names: a wrong answer here is a silently wrong binary,
//! not a diagnostic. It used to be the `punycode` crate, which is `std`; the FIXME in
//! `rustc_symbol_mangling`'s `v0::push_ident` asking rustc to roll its own had been there
//! since the scheme landed.
//!
//! Only the encoder is here. Nothing in the compiler decodes - that is
//! `rustc-demangle`'s job, and it carries its own decoder.
//!
//! The variable names below are the RFC's own (`n`, `delta`, `bias`, `h`, `b`, `m`, `q`,
//! `t`, `k`), so that the code can be read against section 6.3 line by line.


// `String` arrives with the standard prelude and names no path, so a `std::` search cannot see
// it. That is why this `use` is here and why the crate has to say `alloc` out loud.

use alloc::string::String;

// Bootstring parameters for Punycode, RFC 3492 section 5.
const BASE: u32 = 36;
const TMIN: u32 = 1;
const TMAX: u32 = 26;
const SKEW: u32 = 38;
const DAMP: u32 = 700;
const INITIAL_BIAS: u32 = 72;
const INITIAL_N: u32 = 128;
const DELIMITER: char = '-';

/// Encode `input` as Punycode. The result is ASCII and carries no `xn--` prefix.
///
/// The basic (ASCII) code points are copied out in order; if there was at least one, a `-`
/// delimiter follows, and then the encoding of the non-basic ones. A string with no ASCII
/// in it therefore contains no `-` at all, which is the case `v0::push_ident` relies on
/// when it rewrites the *last* `-` to `_`: the identifier itself cannot contain a `-`, so
/// the only `-` that can appear is the delimiter.
///
/// `Err(())` means the encoding overflowed a `u32`, which RFC 3492 section 6.4 requires be
/// detected rather than wrapped. It cannot happen for any real identifier.
pub fn encode(input: &str) -> Result<String, ()> {
    let input_len = input.chars().count() as u32;

    // "let h = b = the number of basic code points in the input"; copy them out in order.
    let mut output: String = input.chars().filter(char::is_ascii).collect();
    let b = output.chars().count() as u32;
    let mut h = b;
    if b > 0 {
        output.push(DELIMITER);
    }

    let mut n = INITIAL_N;
    let mut delta: u32 = 0;
    let mut bias = INITIAL_BIAS;

    while h < input_len {
        // "let m = the minimum code point >= n in the input". One exists: `h < input_len`
        // says some code point has not been emitted yet, and every code point below `n`
        // has been.
        let m = input.chars().map(u32::from).filter(|&c| c >= n).min().ok_or(())?;

        // "let delta = delta + (m - n) * (h + 1)", with the overflow check the RFC asks for.
        delta = (m - n).checked_mul(h + 1).and_then(|d| delta.checked_add(d)).ok_or(())?;
        n = m;

        for c in input.chars().map(u32::from) {
            if c < n {
                delta = delta.checked_add(1).ok_or(())?;
            } else if c == n {
                // Represent `delta` as a generalised variable-length integer.
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = threshold(k, bias);
                    if q < t {
                        break;
                    }
                    output.push(encode_digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                output.push(encode_digit(q));
                bias = adapt(delta, h + 1, h == b);
                delta = 0;
                h += 1;
            }
        }

        delta = delta.checked_add(1).ok_or(())?;
        n += 1;
    }

    Ok(output)
}

/// The threshold `t` for position `k`, RFC 3492 section 3.3: `TMIN` below the bias window,
/// `TMAX` above it, `k - bias` inside.
fn threshold(k: u32, bias: u32) -> u32 {
    if k <= bias + TMIN {
        TMIN
    } else if k >= bias + TMAX {
        TMAX
    } else {
        k - bias
    }
}

/// Bias adaptation, RFC 3492 section 6.1.
///
/// The divisions are the reason the RFC's own reference code is careful here: they are
/// integer divisions and the whole scheme's output depends on the exact truncation.
fn adapt(delta: u32, num_points: u32, first_time: bool) -> u32 {
    let mut delta = if first_time { delta / DAMP } else { delta / 2 };
    delta += delta / num_points;

    let mut k = 0;
    while delta > ((BASE - TMIN) * TMAX) / 2 {
        delta /= BASE - TMIN;
        k += BASE;
    }

    k + (((BASE - TMIN + 1) * delta) / (delta + SKEW))
}

/// Digit `d` in `0..36` as its ASCII character: `0..=25` are `a..=z`, `26..=35` are `0..=9`.
fn encode_digit(d: u32) -> char {
    debug_assert!(d < BASE);
    let c = d + 22 + if d < 26 { 75 } else { 0 };
    c as u8 as char
}
