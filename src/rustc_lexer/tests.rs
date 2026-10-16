use alloc::string::String;

use expect_test::{Expect, expect};

use super::*;

fn check_raw_str(s: &str, expected: Result<u8, RawStrError>) {
    let s = &format!("r{}", s);
    let mut cursor = Cursor::new(s, FrontmatterAllowed::No);
    cursor.bump();
    let res = cursor.raw_double_quoted_string(0);
    assert_eq!(res, expected);
}

#[test]
fn test_naked_raw_str() {
    check_raw_str(r#""abc""#, Ok(0));
}

#[test]
fn test_raw_no_start() {
    check_raw_str(r##""abc"#"##, Ok(0));
}

#[test]
fn test_too_many_terminators() {
    // this error is handled in the parser later
    check_raw_str(r###"#"abc"##"###, Ok(1));
}

#[test]
fn test_unterminated() {
    check_raw_str(
        r#"#"abc"#,
        Err(RawStrError::NoTerminator { expected: 1, found: 0, possible_terminator_offset: None }),
    );
    check_raw_str(
        r###"##"abc"#"###,
        Err(RawStrError::NoTerminator {
            expected: 2,
            found: 1,
            possible_terminator_offset: Some(7),
        }),
    );
    // We're looking for "# not just any #
    check_raw_str(
        r###"##"abc#"###,
        Err(RawStrError::NoTerminator { expected: 2, found: 0, possible_terminator_offset: None }),
    )
}

#[test]
fn test_invalid_start() {
    check_raw_str(r##"#~"abc"#"##, Err(RawStrError::InvalidStarter { bad_char: '~' }));
}

#[test]
fn test_unterminated_no_pound() {
    // https://github.com/rust-lang/rust/issues/70677
    check_raw_str(
        r#"""#,
        Err(RawStrError::NoTerminator { expected: 0, found: 0, possible_terminator_offset: None }),
    );
}

#[test]
fn test_too_many_hashes() {
    let max_count = u8::MAX;
    let hashes1 = "#".repeat(max_count as usize);
    let hashes2 = "#".repeat(max_count as usize + 1);
    let middle = "\"abc\"";
    let s1 = [&hashes1, middle, &hashes1].join("");
    let s2 = [&hashes2, middle, &hashes2].join("");

    // Valid number of hashes (255 = 2^8 - 1 = u8::MAX).
    check_raw_str(&s1, Ok(255));

    // One more hash sign (256 = 2^8) becomes too many.
    check_raw_str(&s2, Err(RawStrError::TooManyDelimiters { found: u32::from(max_count) + 1 }));
}

// https://github.com/rust-lang/rust/issues/70528
#[test]
fn test_valid_shebang() {
    let input = "#!/bin/bash";
    assert_eq!(strip_shebang(input), Some(input.len()));

    let input = "#![attribute]";
    assert_eq!(strip_shebang(input), None);

    let input = "#!    /bin/bash";
    assert_eq!(strip_shebang(input), Some(input.len()));

    let input = "#!    [attribute]";
    assert_eq!(strip_shebang(input), None);

    let input = "#! /* blah */  /bin/bash";
    assert_eq!(strip_shebang(input), Some(input.len()));

    let input = "#! /* blah */  [attribute]";
    assert_eq!(strip_shebang(input), None);

    let input = "#! // blah\n/bin/bash";
    assert_eq!(strip_shebang(input), Some(10)); // strip up to the newline

    let input = "#! // blah\n[attribute]";
    assert_eq!(strip_shebang(input), None);

    let input = "#! /* blah\nblah\nblah */  /bin/bash";
    assert_eq!(strip_shebang(input), Some(10));

    let input = "#! /* blah\nblah\nblah */  [attribute]";
    assert_eq!(strip_shebang(input), None);

    let input = "#!\n/bin/sh";
    assert_eq!(strip_shebang(input), Some(2));

    let input = "#!\n[attribute]";
    assert_eq!(strip_shebang(input), None);

    // Because shebangs are interpreted by the kernel, they must be on the first line
    let input = "\n#!/bin/bash";
    assert_eq!(strip_shebang(input), None);

    let input = "\n#![attribute]";
    assert_eq!(strip_shebang(input), None);
}

fn check_lexing(src: &str, frontmatter_allowed: FrontmatterAllowed, expect: Expect) {
    let actual: String =
        tokenize(src, frontmatter_allowed).map(|token| format!("{:?}\n", token)).collect();
    expect.assert_eq(&actual)
}

#[test]
fn smoke_test() {
    check_lexing(
        "/* my source file */ fn main() { println!(\"zebra\"); }\n",
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: BlockComment { doc_style: None, terminated: true }, len: 20 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Ident, len: 2 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Ident, len: 4 }
            Token { kind: OpenParen, len: 1 }
            Token { kind: CloseParen, len: 1 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: OpenBrace, len: 1 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Ident, len: 7 }
            Token { kind: Bang, len: 1 }
            Token { kind: OpenParen, len: 1 }
            Token { kind: Literal { kind: Str { terminated: true }, suffix_start: 7 }, len: 7 }
            Token { kind: CloseParen, len: 1 }
            Token { kind: Semi, len: 1 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: CloseBrace, len: 1 }
            Token { kind: Whitespace, len: 1 }
        "#]],
    )
}

#[test]
fn comment_flavors() {
    check_lexing(
        r"
// line
//// line as well
/// outer doc line
//! inner doc line
/* block */
/**/
/*** also block */
/** outer doc block */
/*! inner doc block */
",
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: Whitespace, len: 1 }
            Token { kind: LineComment { doc_style: None }, len: 7 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: LineComment { doc_style: None }, len: 17 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: LineComment { doc_style: Some(Outer) }, len: 18 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: LineComment { doc_style: Some(Inner) }, len: 18 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: BlockComment { doc_style: None, terminated: true }, len: 11 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: BlockComment { doc_style: None, terminated: true }, len: 4 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: BlockComment { doc_style: None, terminated: true }, len: 18 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: BlockComment { doc_style: Some(Outer), terminated: true }, len: 22 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: BlockComment { doc_style: Some(Inner), terminated: true }, len: 22 }
            Token { kind: Whitespace, len: 1 }
        "#]],
    )
}

#[test]
fn nested_block_comments() {
    check_lexing(
        "/* /* */ */'a'",
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: BlockComment { doc_style: None, terminated: true }, len: 11 }
            Token { kind: Literal { kind: Char { terminated: true }, suffix_start: 3 }, len: 3 }
        "#]],
    )
}

#[test]
fn characters() {
    check_lexing(
        "'a' ' ' '\\n'",
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: Literal { kind: Char { terminated: true }, suffix_start: 3 }, len: 3 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Char { terminated: true }, suffix_start: 3 }, len: 3 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Char { terminated: true }, suffix_start: 4 }, len: 4 }
        "#]],
    );
}

#[test]
fn lifetime() {
    check_lexing(
        "'abc",
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: Lifetime { starts_with_number: false }, len: 4 }
        "#]],
    );
}

#[test]
fn raw_string() {
    check_lexing(
        "r###\"\"#a\\b\x00c\"\"###",
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: Literal { kind: RawStr { n_hashes: Some(3) }, suffix_start: 17 }, len: 17 }
        "#]],
    )
}

#[test]
fn literal_suffixes() {
    check_lexing(
        r####"
'a'
b'a'
"a"
b"a"
1234
0b101
0xABC
1.0
1.0e10
2us
r###"raw"###suffix
br###"raw"###suffix
"####,
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Char { terminated: true }, suffix_start: 3 }, len: 3 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Byte { terminated: true }, suffix_start: 4 }, len: 4 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Str { terminated: true }, suffix_start: 3 }, len: 3 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: ByteStr { terminated: true }, suffix_start: 4 }, len: 4 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Int { base: Decimal, empty_int: false }, suffix_start: 4 }, len: 4 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Int { base: Binary, empty_int: false }, suffix_start: 5 }, len: 5 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Int { base: Hexadecimal, empty_int: false }, suffix_start: 5 }, len: 5 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Float { base: Decimal, empty_exponent: false }, suffix_start: 3 }, len: 3 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Float { base: Decimal, empty_exponent: false }, suffix_start: 6 }, len: 6 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Int { base: Decimal, empty_int: false }, suffix_start: 1 }, len: 3 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: RawStr { n_hashes: Some(3) }, suffix_start: 12 }, len: 18 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: RawByteStr { n_hashes: Some(3) }, suffix_start: 13 }, len: 19 }
            Token { kind: Whitespace, len: 1 }
        "#]],
    )
}

#[test]
fn frontmatter_allowed() {
    check_lexing(
        r#"
---cargo
[dependencies]
clap = "4"
---

fn main() {}
"#,
        FrontmatterAllowed::Yes,
        expect![[r#"
            Token { kind: Whitespace, len: 1 }
            Token { kind: Frontmatter { has_invalid_preceding_whitespace: false, invalid_infostring: false }, len: 38 }
            Token { kind: Whitespace, len: 2 }
            Token { kind: Ident, len: 2 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Ident, len: 4 }
            Token { kind: OpenParen, len: 1 }
            Token { kind: CloseParen, len: 1 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: OpenBrace, len: 1 }
            Token { kind: CloseBrace, len: 1 }
            Token { kind: Whitespace, len: 1 }
        "#]],
    )
}

#[test]
fn frontmatter_disallowed() {
    check_lexing(
        r#"
---cargo
[dependencies]
clap = "4"
---

fn main() {}
"#,
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: Whitespace, len: 1 }
            Token { kind: Minus, len: 1 }
            Token { kind: Minus, len: 1 }
            Token { kind: Minus, len: 1 }
            Token { kind: Ident, len: 5 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: OpenBracket, len: 1 }
            Token { kind: Ident, len: 12 }
            Token { kind: CloseBracket, len: 1 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Ident, len: 4 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Eq, len: 1 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Str { terminated: true }, suffix_start: 3 }, len: 3 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Minus, len: 1 }
            Token { kind: Minus, len: 1 }
            Token { kind: Minus, len: 1 }
            Token { kind: Whitespace, len: 2 }
            Token { kind: Ident, len: 2 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Ident, len: 4 }
            Token { kind: OpenParen, len: 1 }
            Token { kind: CloseParen, len: 1 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: OpenBrace, len: 1 }
            Token { kind: CloseBrace, len: 1 }
            Token { kind: Whitespace, len: 1 }
        "#]],
    )
}

#[test]
fn byte_skipping_scans() {
    // Strings, block comments, whitespace and identifiers jump over bytes; each stop
    // (escapes, nesting, a multi-byte char, end of file) must land where bumping one char
    // at a time lands.
    check_lexing(
        "\"a\\\"\u{3b2}b\\\\\"x \"ab\\",
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: Literal { kind: Str { terminated: true }, suffix_start: 10 }, len: 11 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Literal { kind: Str { terminated: false }, suffix_start: 4 }, len: 4 }
        "#]],
    );
    check_lexing(
        "/* \u{3b2} /* */ * / */ab /* \u{e9}",
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: BlockComment { doc_style: None, terminated: true }, len: 18 }
            Token { kind: Ident, len: 2 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: BlockComment { doc_style: None, terminated: false }, len: 5 }
        "#]],
    );
    check_lexing(
        " \u{2028}\t\u{85}x a\u{e9}_1\u{431} ",
        FrontmatterAllowed::No,
        expect![[r#"
            Token { kind: Whitespace, len: 7 }
            Token { kind: Ident, len: 1 }
            Token { kind: Whitespace, len: 1 }
            Token { kind: Ident, len: 7 }
            Token { kind: Whitespace, len: 1 }
        "#]],
    );
}

#[test]
fn swar_masks_match_byte_predicates() {
    // Every byte value at every lane, among neighbours that pass and neighbours that do not:
    // each mask bit is the scalar answer for the corresponding byte.
    for filler in [b'a', b' ', b'!', 0xC3, 0x00, 0x7F] {
        for lane in 0..8 {
            for b in 0..=255u8 {
                let mut bytes = [filler; 8];
                bytes[lane] = b;
                let masks = simd_classify::classify_partial(&bytes);
                let id = masks.ident_cont;
                let ws = masks.whitespace;
                assert_eq!(id >> bytes.len(), 0);
                assert_eq!(ws >> bytes.len(), 0);
                for (k, &byte) in bytes.iter().enumerate() {
                    let bit = 1u64 << k;
                    let want_id = byte.is_ascii_alphanumeric() || byte == b'_';
                    let want_ws = byte < 0x80 && is_whitespace(byte as char);
                    assert_eq!(id & bit != 0, want_id, "id byte {byte:#x} lane {k}");
                    assert_eq!(ws & bit != 0, want_ws, "ws byte {byte:#x} lane {k}");
                }
            }
        }
    }
}

/// Inputs that cross the eight-byte blocks at every offset, with non-ASCII chars inside and
/// at the edges of identifier and whitespace runs.
fn scan_inputs() -> alloc::vec::Vec<String> {
    let pieces = [
        "a",
        "Z",
        "_",
        "9",
        "\u{e9}",
        "\u{431}",
        "\u{1F600}",
        " ",
        "\t",
        "\n",
        "\r",
        "\x0b",
        "\x0c",
        "\u{85}",
        "\u{2028}",
        "\u{200e}",
        "\u{a0}",
        "-",
        "'",
        "\"",
        "#",
        "\0",
        "\x7f",
        "\\",
        "`",
    ];
    let mut inputs = alloc::vec::Vec::new();
    for lead in 0..12 {
        for p in pieces {
            for q in pieces {
                let mut s = String::new();
                for _ in 0..lead {
                    s.push_str(if lead % 2 == 0 { "x" } else { " " });
                }
                s.push_str(p);
                for _ in 0..(11 - lead) {
                    s.push_str(if lead % 3 == 0 { "_" } else { "\t" });
                }
                s.push_str(q);
                inputs.push(s);
            }
        }
    }
    inputs
}

#[test]
fn block_scans_match_char_scans() {
    for s in scan_inputs() {
        for start in 0..s.len() {
            if !s.is_char_boundary(start) {
                continue;
            }
            let rest = &s[start..];

            let mut fast = Cursor::new(rest, FrontmatterAllowed::No);
            fast.eat_id_continue();
            let mut slow = Cursor::new(rest, FrontmatterAllowed::No);
            slow.eat_while(is_id_continue);
            assert_eq!(fast.as_str().len(), slow.as_str().len(), "id {rest:?}");

            let mut fast = Cursor::new(rest, FrontmatterAllowed::No);
            fast.eat_whitespace();
            let mut slow = Cursor::new(rest, FrontmatterAllowed::No);
            slow.eat_while(is_whitespace);
            assert_eq!(fast.as_str().len(), slow.as_str().len(), "ws {rest:?}");
        }
    }
}

#[test]
fn ascii_dispatch_matches_general() {
    // Every ASCII first byte, followed by every continuation that changes an arm's choice,
    // lexes to the same tokens through the byte dispatch as through the char dispatch.
    let tails = [
        "",
        "/",
        "*",
        "!",
        "=",
        "#",
        "\"",
        "'",
        "r",
        "r#",
        "r#x",
        "r\"a\"",
        "#\"a\"#",
        "x",
        "0",
        "0x1f_u8",
        "0b",
        "1.5e+3f32",
        "1..2",
        ".x",
        "\u{e9}",
        "\u{1F600}",
        " ",
        "\n",
        "'a'",
        "a'",
        "\\n'",
        "b'a'",
        "\"s\\\"t\"suf",
        "//x\n",
        "/**/ ",
        "/* /* */ */",
        "_",
    ];
    let mut inputs: alloc::vec::Vec<String> = scan_inputs();
    for b in 0..0x80u8 {
        for t in tails {
            let mut s = String::new();
            s.push(b as char);
            s.push_str(t);
            inputs.push(s);
        }
    }
    for s in inputs {
        let mut fast = Cursor::new(&s, FrontmatterAllowed::No);
        let mut general = Cursor::new(&s, FrontmatterAllowed::No);
        loop {
            let a = fast.advance_token();
            let b = general.advance_token_general();
            assert_eq!((a.kind, a.len), (b.kind, b.len), "{s:?}");
            assert_eq!(fast.as_str().len(), general.as_str().len(), "{s:?}");
            if a.kind == TokenKind::Eof {
                break;
            }
        }
    }
}

#[test]
fn r24_classify_dispatch_matches_swar_at_every_lane() {
    use super::simd_classify::*;
    assert_eq!(low_bits_mask(0), 0);
    assert_eq!(low_bits_mask(63), u64::MAX >> 1);
    assert_eq!(low_bits_mask(64), u64::MAX);
    assert_eq!(low_bits_mask(65), u64::MAX);
    for lane in 0..64 {
        for byte in 0..=u8::MAX {
            let mut block = [b'a'; 64];
            block[lane] = byte;
            let masks = classify_block(&block);
            assert_eq!(masks, classify_block_swar(&block));
            let bit = 1u64 << lane;
            assert_eq!(masks.whitespace & bit != 0, matches!(byte, 9..=13 | 32));
            assert_eq!(masks.ident_cont & bit != 0, byte.is_ascii_alphanumeric() || byte == b'_');
            assert_eq!(masks.digit & bit != 0, byte.is_ascii_digit());
            assert_eq!(masks.non_ascii & bit != 0, !byte.is_ascii());
            assert_eq!(masks.bidi_lead & bit != 0, matches!(byte, 0xd8 | 0xe2));
            assert_eq!(mask_ascii_whitespace_64(&block), (masks.whitespace, masks.non_ascii));
            assert_eq!(mask_newline_and_bidi_64(&block), (masks.newline, masks.bidi_lead));
            assert_eq!(
                mask_string_body_64(&block),
                (masks.quote, masks.backslash, masks.cr, masks.non_ascii)
            );
            assert_eq!(mask_ident_cont_64(&block), (masks.ident_cont, masks.non_ascii));
            assert_eq!(
                mask_digits_and_underscore_64(&block),
                masks.digit | if byte == b'_' { bit } else { 0 }
            );
        }
    }
    for len in 0..=64 {
        let masks = classify_partial(&[b' '; 64][..len]);
        assert_eq!(masks.whitespace, low_bits_mask(len));
        assert_eq!(masks.ident_cont, 0);
        assert_eq!(masks.non_ascii, 0);
    }
}

#[test]
fn r24_scanners_cross_63_64_65_bytes() {
    use super::simd_ident_num::*;
    use super::simd_scan::*;
    for n in [63, 64, 65] {
        let padding = "a".repeat(n);
        let whitespace = format!("{}\n\tx", " ".repeat(n));
        let mut newlines = alloc::vec::Vec::new();
        assert_eq!(scan_ascii_whitespace(whitespace.as_bytes()), n + 2);
        assert_eq!(
            scan_ascii_whitespace_with_newlines(whitespace.as_bytes(), 17, &mut newlines),
            n + 2
        );
        assert_eq!(newlines, [17 + n as u32]);
        assert_eq!(scan_ascii_ident_full(padding.as_bytes()), Some(IdentScan::AsciiOnly(n)));
        assert_eq!(
            scan_ascii_ident_tail(format!("{padding}é").as_bytes()),
            IdentScan::NeedsUnicodeAt(n)
        );
        for scan in [scan_decimal_digits, scan_hex_digits, scan_octal_digits, scan_binary_digits] {
            assert_eq!(scan(format!("{}!", "1".repeat(n)).as_bytes()), (n, true));
            assert_eq!(scan("___!".as_bytes()), (3, false));
        }
        for bidi in ['\u{202e}', '\u{2066}', '\u{061c}'] {
            let body = format!("{padding}{bidi}");
            assert!(contains_text_flow_control_char(body.as_bytes()));
            assert!(is_text_flow_control_at(body.as_bytes(), n));
            assert!(!is_text_flow_control_at(body.as_bytes(), body.len()));
            let line = format!("{body}\nnext");
            assert_eq!(
                scan_line_comment(line.as_bytes()),
                LineCommentScan { len: body.len(), has_bidi: true, has_cr: false }
            );
            let nested = format!("{body}/* nested */tail*/rest");
            let block = scan_block_comment(nested.as_bytes());
            assert!(block.terminated && block.has_bidi);
            assert!(!block.has_cr);
            assert_eq!(block.len, nested.len() - 4);
            let quoted = format!("{body}\\\"tail\r\"rest");
            let string = scan_double_quoted_string(quoted.as_bytes());
            assert_eq!(string.len, quoted.len() - 4);
            assert!(string.terminated && string.has_bidi && string.has_cr);
            assert!(!string.clean_ascii_no_escape);
            let raw = format!("{body}\r\"#false\"###rest");
            assert_eq!(scan_raw_string_body(raw.as_bytes(), 3), Some((raw.len() - 4, true, true)));
            assert_eq!(scan_raw_string_body(body.as_bytes(), 3), None);
        }
        assert_eq!(scan_raw_string_body(b"\"", 256), None);
        assert!(!scan_block_comment(padding.as_bytes()).terminated);
        assert!(!scan_double_quoted_string(padding.as_bytes()).terminated);
        assert_eq!(
            scan_ascii_lifetime(b"a"),
            LifetimeScan::Lifetime { len: 1, starts_with_number: false }
        );
        for fallback in ["r#name", "a'", "é", "aé"] {
            assert_eq!(scan_ascii_lifetime(fallback.as_bytes()), LifetimeScan::CharOrOther);
        }
    }
}

/// The leading-word paths of the whitespace and identifier scanners stop exactly where a byte
/// at a time would: every stop byte at every position through the words, the first blocks and
/// the tail, with and without input after the stop.
#[test]
fn leading_word_scans_match_bytewise() {
    use super::simd_ident_num::*;
    use super::simd_scan::*;
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let space = |b: u8| matches!(b, 9..=13 | 32);
    for at in 0..=80 {
        for stop in 0..=u8::MAX {
            for trailing in [0, 1, 7, 70] {
                let mut text = alloc::vec![b'x'; at];
                text.push(stop);
                text.extend(core::iter::repeat_n(b'y', trailing));
                let want = text.iter().position(|&b| !ident(b)).unwrap_or(text.len());
                let got = match scan_ascii_ident_tail(&text) {
                    IdentScan::AsciiOnly(len) => {
                        assert!(text.get(len).is_none_or(u8::is_ascii));
                        len
                    }
                    IdentScan::NeedsUnicodeAt(len) => {
                        assert!(!text[len].is_ascii());
                        len
                    }
                };
                assert_eq!(got, want, "identifier, stop {stop:#x} at {at}, {trailing} after");
                text[..at].fill(b' ');
                text[at + 1..].fill(b'\t');
                let want = text.iter().position(|&b| !space(b)).unwrap_or(text.len());
                assert_eq!(
                    scan_ascii_whitespace(&text),
                    want,
                    "whitespace, stop {stop:#x} at {at}, {trailing} after"
                );
            }
        }
    }
}

#[test]
fn w4_advance_token_with_meta_tracks_newlines_and_raw_strings() {
    let source = " \r\n// comment\n/* block\r\n*/ \"str\\\"ing\" r#\"raw\r\ntext\"# 'a";
    let mut cursor = Cursor::new(source, FrontmatterAllowed::No);
    let mut tokens = alloc::vec::Vec::new();
    let mut newlines = alloc::vec::Vec::new();
    let mut pos = 0;
    while !cursor.is_eof() {
        let (token, meta) = cursor.advance_token_with_meta(pos, &mut newlines);
        pos += token.len;
        tokens.push((token, meta));
    }
    assert!(tokens.iter().any(|(token, _)| token.kind == TokenKind::Whitespace));
    assert!(tokens.iter().any(|(token, _)| matches!(token.kind, TokenKind::LineComment { .. })));
    assert!(tokens.iter().any(|(token, _)| matches!(token.kind, TokenKind::BlockComment { .. })));
    assert!(tokens.iter().any(|(token, _)| matches!(
        token.kind,
        TokenKind::Literal { kind: LiteralKind::Str { .. }, .. }
    )));
    assert!(tokens.iter().any(|(token, meta)| matches!(
        token.kind,
        TokenKind::Literal { kind: LiteralKind::RawStr { .. }, .. }
    ) && meta.has_cr));
    assert!(tokens.iter().any(|(token, _)| matches!(token.kind, TokenKind::Lifetime { .. })));
    let expected_newlines: alloc::vec::Vec<u32> = source
        .bytes()
        .enumerate()
        .filter_map(|(i, byte)| (byte == b'\n').then_some(i as u32))
        .collect();
    assert_eq!(newlines, expected_newlines);
}

#[test]
fn r24_boundary_lexing_matches_general_and_structural_index() {
    use super::structural_tape::*;
    for n in [63, 64, 65] {
        let padding = "x".repeat(n);
        let source = format!(
            "{{ /*{padding}/* nested {{ */\n*/ let {padding} = r###\"{{[\n\"#}}\"###; \"{padding}\\\"}}\"; [123_456] }}"
        );
        let mut fast = Cursor::new(&source, FrontmatterAllowed::No);
        let mut slow = Cursor::new(&source, FrontmatterAllowed::No);
        loop {
            let a = fast.advance_token();
            let b = slow.advance_token_general();
            assert_eq!((a.kind, a.len), (b.kind, b.len));
            if a.kind == TokenKind::Eof {
                break;
            }
        }
        let index = build_structural_index(&source);
        assert!(index.balanced);
        assert_eq!(index.groups.len(), 2);
        assert_eq!(index.groups[0].delim, DelimKind::Brace);
        assert_eq!(index.groups[0].parent, u32::MAX);
        assert_eq!(index.groups[0].close_byte as usize, source.len() - 1);
        assert_eq!(index.groups[1].delim, DelimKind::Bracket);
        assert_eq!(index.groups[1].parent, 0);
        let expected: alloc::vec::Vec<u32> = source
            .bytes()
            .enumerate()
            .filter_map(|(i, b)| (b == b'\n').then_some(i as u32))
            .collect();
        assert_eq!(index.newlines, expected);
        assert_eq!(scan_newline_offsets(&source), expected);
        assert_eq!(index.depth_zero_cuts, [source.len() as u32]);
    }
    for invalid in ["{]", "{", "/* unterminated", "\"unterminated", "r#\"unterminated"] {
        assert!(!build_structural_index(invalid).balanced, "{invalid:?}");
    }
}

#[test]
fn r24_keyword_lookup_is_exact_and_preinterned() {
    use super::keyword_phf::*;
    crate::rustc_span::create_default_session_globals_then(|| {
        for text in ["fn", "mod", "async", "await", "self", "Self", "crate", "where", "yield"] {
            assert_eq!(
                classify_keyword_bytes(text.as_bytes()),
                Some(crate::rustc_span::Symbol::intern(text))
            );
            assert!(is_strict_or_reserved_keyword(text.as_bytes()));
            assert_eq!(classify_keyword_bytes(format!("{text}\0").as_bytes()), None);
        }
        for text in ["'static", "'_"] {
            assert_eq!(
                classify_lifetime_keyword_bytes(text.as_bytes()),
                Some(crate::rustc_span::Symbol::intern(text))
            );
        }
        for text in ["u32", "Box", "Drop"] {
            assert_eq!(
                classify_preinterned_ident_bytes(text.as_bytes()),
                Some(crate::rustc_span::Symbol::intern(text))
            );
        }
        assert_eq!(classify_keyword_bytes(b"r24_unknown_identifier"), None);
        assert_eq!(classify_keyword_bytes("é".as_bytes()), None);
        assert_eq!(pack_ascii_u64(b""), Some(0));
        assert_eq!(pack_ascii_u64(b"ab"), Some(u64::from_le_bytes(*b"ab\0\0\0\0\0\0")));
        assert_eq!(pack_ascii_u64(b"123456789"), None);
        assert_eq!(pack_ascii_u128(b"12345678901234567"), None);
        assert_eq!(pack_ascii_u128("é".as_bytes()), None);
        assert_eq!(pack_ascii_u128(b"ab"), Some(0x6261));
    });
}
