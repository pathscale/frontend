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

macro_rules! test {
    (case: $test_name:ident,
     text: $text:expr,
     lines: $lines:expr,
     multi_byte_chars: $multi_byte_chars:expr,) => {
        #[test]
        fn $test_name() {
            let (lines, multi_byte_chars) = analyze_source_file($text);

            let expected_lines: Vec<RelativeBytePos> =
                $lines.into_iter().map(RelativeBytePos).collect();

            assert_eq!(lines, expected_lines);

            let expected_mbcs: Vec<MultiByteChar> = $multi_byte_chars
                .into_iter()
                .map(|(pos, bytes)| MultiByteChar { pos: RelativeBytePos(pos), bytes })
                .collect();

            assert_eq!(multi_byte_chars, expected_mbcs);
        }
    };
}

test!(
    case: empty_text,
    text: "",
    lines: vec![],
    multi_byte_chars: vec![],
);

test!(
    case: newlines_short,
    text: "a\nc",
    lines: vec![0, 2],
    multi_byte_chars: vec![],
);

test!(
    case: newlines_long,
    text: "012345678\nabcdef012345678\na",
    lines: vec![0, 10, 26],
    multi_byte_chars: vec![],
);

test!(
    case: newline_and_multi_byte_char_in_same_chunk,
    text: "01234β789\nbcdef0123456789abcdef",
    lines: vec![0, 11],
    multi_byte_chars: vec![(5, 2)],
);

test!(
    case: newline_and_control_char_in_same_chunk,
    text: "01234\u{07}6789\nbcdef0123456789abcdef",
    lines: vec![0, 11],
    multi_byte_chars: vec![],
);

test!(
    case: multi_byte_char_short,
    text: "aβc",
    lines: vec![0],
    multi_byte_chars: vec![(1, 2)],
);

test!(
    case: multi_byte_char_long,
    text: "0123456789abcΔf012345β",
    lines: vec![0],
    multi_byte_chars: vec![(13, 2), (22, 2)],
);

test!(
    case: multi_byte_char_across_chunk_boundary,
    text: "0123456789abcdeΔ123456789abcdef01234",
    lines: vec![0],
    multi_byte_chars: vec![(15, 2)],
);

test!(
    case: multi_byte_char_across_chunk_boundary_tail,
    text: "0123456789abcdeΔ....",
    lines: vec![0],
    multi_byte_chars: vec![(15, 2)],
);

test!(
    case: non_narrow_short,
    text: "0\t2",
    lines: vec![0],
    multi_byte_chars: vec![],
);

test!(
    case: non_narrow_long,
    text: "01\t3456789abcdef01234567\u{07}9",
    lines: vec![0],
    multi_byte_chars: vec![],
);

test!(
    case: output_offset_all,
    text: "01\t345\n789abcΔf01234567\u{07}9\nbcΔf",
    lines: vec![0, 7, 27],
    multi_byte_chars: vec![(13, 2), (29, 2)],
);

/// The scalar decoder run over the whole text, with the same trailing-line trim as
/// `analyze_source_file`: the reference every vector path must match byte for byte.
fn analyze_scalar(src: &str) -> (Vec<RelativeBytePos>, Vec<MultiByteChar>) {
    let mut lines = vec![RelativeBytePos::from_u32(0)];
    let mut multi_byte_chars = vec![];
    let overflow = analyze_source_file_generic(
        src,
        src.len(),
        RelativeBytePos::from_u32(0),
        &mut lines,
        &mut multi_byte_chars,
    );
    assert_eq!(overflow, 0);
    if lines.last() == Some(&RelativeBytePos::from_usize(src.len())) {
        lines.pop();
    }
    (lines, multi_byte_chars)
}

fn assert_matches_scalar(src: &str) {
    assert_eq!(analyze_source_file(src), analyze_scalar(src), "input {src:?}");
}

#[test]
fn vector_matches_scalar_on_lengths() {
    // Every length around the chunk sizes, all newlines, no newlines, and mixed.
    for len in 0..=70 {
        let plain: String = (0..len).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        assert_matches_scalar(&plain);
        let newlines: String = "\n".repeat(len);
        assert_matches_scalar(&newlines);
        let mixed: String =
            (0..len).map(|i| if i % 3 == 2 { '\n' } else { (b'0' + (i % 10) as u8) as char }).collect();
        assert_matches_scalar(&mixed);
    }
    for text in ["", "\n", "a", "0123456789abcde", "0123456789abcdef", "0123456789abcdef0"] {
        assert_matches_scalar(text);
    }
}

#[test]
fn vector_matches_scalar_on_multibyte_at_every_offset() {
    // Each width of character (2, 3 and 4 bytes) placed at every offset over three chunks,
    // so it lands wholly inside a chunk, straddles each boundary position, and ends in
    // the tail; a newline right after it checks the line positions that follow.
    for c in ['\u{e9}', '\u{394}', '\u{2028}', '\u{20ac}', '\u{1f600}'] {
        for prefix in 0..48 {
            for suffix in [0usize, 1, 5, 15, 16, 17, 33] {
                let mut text = "x".repeat(prefix);
                text.push(c);
                text.push('\n');
                text.push_str(&"y\n".repeat(suffix / 2));
                text.push_str(&"z".repeat(suffix % 2));
                assert_matches_scalar(&text);
            }
        }
    }
}

#[test]
fn vector_matches_scalar_on_crafted_text() {
    let texts = [
        // Line endings before and after normalization: `\r` never ends a line, `\n` does.
        "fn main() {\n    let a = 1;\r\n}\n",
        "line one\nline two\r\nline three\rstill three\n",
        // Newline as the first and last byte of a chunk.
        "\n123456789abcdef\n123456789abcde\n",
        // Back-to-back multi-byte characters across two boundaries.
        "0123456789abcd\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\n",
        "\u{394}\u{394}\u{394}\u{394}\u{394}\u{394}\u{394}\u{394}\u{394}\n\u{394}",
        // Control characters are ASCII and take the fast path.
        "\u{07}\u{1b}\u{7f}\t\u{0b}\u{0c}0123456789\n\u{7f}",
        // Ends exactly on a chunk boundary with a newline.
        "0123456789abcde\n",
        "0123456789abcdef0123456789abcde\n",
    ];
    for text in texts {
        assert_matches_scalar(text);
    }
    // A larger mixed file, so many chunks alternate between the two paths.
    let mut big = String::new();
    for i in 0..500 {
        big.push_str("    let value_");
        big.push_str(&i.to_string());
        big.push_str(" = \"");
        if i % 7 == 0 {
            big.push_str("\u{3b1}\u{3b2}\u{3b3}");
        }
        if i % 11 == 0 {
            big.push('\u{1f980}');
        }
        big.push_str("\";\r\n");
    }
    assert_matches_scalar(&big);
}
