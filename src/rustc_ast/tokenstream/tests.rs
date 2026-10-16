use crate::rustc_span::DUMMY_SP;

use crate::rustc_ast::token::TokenKind;
use crate::rustc_ast::tokenstream::TokenStream;

#[test]
fn test_token_stream_iter() {
    let ts = TokenStream::token_alone(TokenKind::Eq, DUMMY_SP);
    assert_eq!(ts.len(), 1);

    let iter = ts.iter();
    assert_eq!(iter.size_hint(), (1, Some(1)));
}

#[test]
fn r24_compact_token_layout_and_packing() {
    compact_delimiter_groups_pack_twenty_bits();
    short_symbol_cache_keys_preserve_exact_source_bytes();
    arena_bulk_moves::bulk_moves_preserve_ownership_and_scratch_prefix();
    use super::Spacing;
    use super::compact_token::*;
    use super::source_token::{SourceKind, SourceToken};
    assert_eq!(core::mem::size_of::<CompactToken>(), 8);
    assert_eq!(core::mem::align_of::<CompactToken>(), 8);
    assert_eq!(core::mem::size_of::<SourceToken>(), 16);
    for raw in 0..=66 {
        let kind = SourceKind::from_raw(raw);
        assert_eq!(kind as u8, raw);
        for len in [0, 1, 4094, 4095, 4096, 0x7ffe] {
            for spacing in [Spacing::Alone, Spacing::Joint, Spacing::JointHidden] {
                for (group, aux) in [(false, false), (true, false), (false, true), (true, true)] {
                    let token = CompactToken::new(91, len, kind, spacing, 1, group, aux);
                    assert_eq!(token.lo(), 91);
                    assert_eq!(token.kind(), kind);
                    assert_eq!(token.aux(), 1);
                    assert_eq!(token.inline_len(), (len < 4095).then_some(len));
                    assert_eq!(token.spacing(), spacing);
                    assert_eq!(token.has_group(), group);
                    assert_eq!(token.has_aux_u32(), aux);
                    assert_ne!(
                        token.len_and_flags & (COMPACT_FLAG_JOINT | COMPACT_FLAG_JOINT_HIDDEN),
                        COMPACT_FLAG_JOINT | COMPACT_FLAG_JOINT_HIDDEN
                    );
                }
            }
        }
    }
    for raw in 67..=255 {
        assert_eq!(SourceKind::from_raw(raw), SourceKind::Eof);
    }
    assert_eq!(SourceKind::Ident as u8, 52);
    assert_eq!(SourceKind::Lifetime as u8, 53);
    assert_eq!(SourceKind::LineDoc as u8, 64);
    assert_eq!(SourceKind::BlockDoc as u8, 65);
}

#[test]
fn r24_preglued_punctuation_matches_source_glue() {
    use super::compact_token::{may_glue_after, try_glue_punct};
    use super::source_token::SourceKind;
    for (text, parts) in [
        ("==", &[SourceKind::Eq, SourceKind::Eq][..]),
        ("::", &[SourceKind::Colon, SourceKind::Colon][..]),
        ("->", &[SourceKind::Minus, SourceKind::Gt][..]),
        ("..=", &[SourceKind::Dot, SourceKind::Dot, SourceKind::Eq][..]),
        (">>=", &[SourceKind::Gt, SourceKind::Gt, SourceKind::Eq][..]),
    ] {
        let expected = parts[1..].iter().fold(parts[0], |a, &b| a.glue(b).unwrap());
        let glued = try_glue_punct(text.as_bytes()).unwrap();
        assert_eq!(glued.kind, expected);
        assert_eq!(glued.len as usize, text.len());
        assert!(!may_glue_after(text.as_bytes(), text.len()));
    }
    for text in ["//comment", "/*comment*/"] {
        let result = try_glue_punct(text.as_bytes());
        assert!(result.is_none_or(|punct| punct.len == 1));
    }
    for text in ["= x", "=/*x*/", "=//x", "=(", "=name", "="] {
        assert!(!may_glue_after(text.as_bytes(), 1));
    }
    assert!(may_glue_after(b"=+", 1));
}

fn r24_tape(source: &str) -> alloc::sync::Arc<super::lexed_tape::LexedTape> {
    super::lexed_tape::LexedTape::build(
        alloc::sync::Arc::new(alloc::string::String::from(source)),
        crate::rustc_span::BytePos(100),
        0,
        crate::rustc_lexer::FrontmatterAllowed::No,
        crate::rustc_span::edition::Edition::Edition2024,
    )
    .expect("valid ASCII source must use the indexed tape")
}

// A hand-built tree oracle keeps the AST tests independent of rustc_parse and
// indexed materialization. The fixture separates every token with whitespace.
fn r24_tree_cursor() -> super::TokenCursor {
    use super::{DelimSpacing, DelimSpan, Spacing, TokenTree};
    use crate::rustc_ast::token::{self, Delimiter, IdentIsRaw};
    use crate::rustc_span::Symbol;
    fn ident(text: &str) -> TokenTree {
        TokenTree::token_alone(token::Ident(Symbol::intern(text), IdentIsRaw::No), DUMMY_SP)
    }
    fn group(delim: Delimiter, trees: alloc::vec::Vec<TokenTree>) -> TokenTree {
        TokenTree::Delimited(
            DelimSpan::dummy(),
            DelimSpacing::new(Spacing::Alone, Spacing::Alone),
            delim,
            TokenStream::new(trees),
        )
    }
    let paren = || group(Delimiter::Parenthesis, alloc::vec![]);
    let body = group(
        Delimiter::Brace,
        alloc::vec![
            ident("call"),
            group(
                Delimiter::Parenthesis,
                alloc::vec![
                    ident("a"),
                    TokenTree::token_alone(token::Comma, DUMMY_SP),
                    group(Delimiter::Bracket, alloc::vec![ident("b")]),
                ]
            ),
            TokenTree::token_alone(token::Semi, DUMMY_SP),
        ],
    );
    super::TokenCursor::new(TokenStream::new(alloc::vec![
        ident("fn"),
        ident("first"),
        paren(),
        body,
        ident("fn"),
        ident("second"),
        paren(),
        group(Delimiter::Brace, alloc::vec![]),
    ]))
}

#[test]
fn r24_indexed_tape_side_arrays_and_cursor_boundaries() {
    use super::on_demand::OnDemandCursor;
    use crate::rustc_ast::token::{self, Delimiter};
    crate::rustc_span::create_default_session_globals_then(|| {
        let source = "{ r#name::call(1, [2]); } tail";
        let tape = r24_tape(source);
        let count = tape.token_count();
        for i in 0..count {
            let kind = tape.kind(i);
            let delimiter = kind.is_open_delim() || kind.is_close_delim();
            assert_eq!(delimiter, tape.token_group(i) != super::lexed_tape::NO_GROUP);
        }
        let raw = (0..count).find(|&i| tape.kind(i) == frontend_lex::Kind::Ident).unwrap();
        // `r#name` at offset 2: the span covers the prefix, the symbol does not.
        assert_eq!((tape.token_span(raw).lo().0, tape.token_span(raw).hi().0), (102, 108));
        assert_eq!(tape.raw_identifier_spans(), [tape.token_span(raw)]);
        assert_eq!(tape.kind(count - 1), frontend_lex::Kind::Eof);
        let mut cursor = OnDemandCursor::from_tape(tape.clone());
        assert_eq!(cursor.look_ahead_token(0), token::Token::dummy());
        for i in 0..tape.token_count() {
            for dist in 1..=8 {
                let idx = (i + dist - 1).min(tape.token_count() - 1);
                assert_eq!(cursor.look_ahead_token(dist), tape.materialize_token(idx).0);
            }
            assert_eq!(cursor.next_and_bump(), tape.materialize_token(i));
        }
        assert_eq!(cursor.next_and_bump().0.kind, token::Eof);
        assert_eq!(cursor.look_ahead_token(usize::MAX).kind, token::Eof);
        let mut cursor = OnDemandCursor::from_tape(tape.clone());
        assert_eq!(cursor.next_and_bump().0.kind, token::OpenBrace);
        let info = cursor.enclosing_group_info().unwrap();
        assert_eq!(info.delim, Delimiter::Brace);
        let group = tape.groups()[0];
        assert_eq!(info.inside_bumps, group.close - group.open - 1);
        assert!(matches!(cursor.look_ahead(4), Some(super::TokenTree::Delimited(..))));
        assert_eq!(cursor.look_ahead_tree_token(4).unwrap().kind, token::OpenParen);
        let mut detached = cursor.detached_for_group(info.group_idx).unwrap();
        assert_eq!(detached.look_ahead_token(1), cursor.look_ahead_token(1));
        detached.bump_to_end();
        assert!(detached.look_ahead(1).is_none());
        assert!(detached.look_ahead_tree_token(1).is_none());
        assert!(detached.look_ahead_past_close_delim().is_none());
        assert_eq!(detached.next_and_bump().0.kind, token::CloseBrace);
        for _ in 0..3 {
            let eof = detached.next_and_bump().0;
            assert_eq!(eof.kind, token::Eof);
            assert_eq!(eof.span, info.close);
        }
        cursor.bump_to_end();
        assert_eq!(cursor.next_and_bump().0.kind, token::CloseBrace);
        let mut after_close = cursor.detached_for_group(info.group_idx).unwrap();
        assert_eq!(after_close.next_and_bump().0.kind, token::Eof);
        assert!(matches!(cursor.next_and_bump().0.kind, token::Ident(..)));
        assert!(cursor.detached_for_group(info.group_idx).is_none());
    });
}

#[test]
fn r24_cursor_adapters_match_tree_oracle() {
    use super::TokenCursor;
    use super::on_demand::OnDemandCursor;
    use crate::rustc_ast::token;
    crate::rustc_span::create_default_session_globals_then(|| {
        let source = "fn first ( ) { call ( a , [ b ] ) ; } fn second ( ) { }";
        // Compare kinds and spacing; absolute indexed spans are checked above.
        let mut trees = r24_tree_cursor();
        let tape = r24_tape(source);
        let mut indexed = OnDemandCursor::from_tape(tape.clone());
        let mut adapter = TokenCursor::from_lexed_tape(tape.clone());
        assert!(adapter.can_parallel_parse());
        assert!(alloc::sync::Arc::ptr_eq(adapter.lexed_tape().unwrap(), &tape));
        loop {
            for dist in 1..=8 {
                assert_eq!(
                    indexed.look_ahead_token(dist).kind,
                    trees.look_ahead_token(dist).unwrap().kind
                );
                assert_eq!(adapter.look_ahead_token(dist).unwrap(), indexed.look_ahead_token(dist));
            }
            for n in 1..=3 {
                let expected = trees.look_ahead(n);
                let actual = indexed.look_ahead(n);
                assert_eq!(actual.is_some(), expected.is_some());
                if let (Some(actual), Some(expected)) = (actual, expected) {
                    assert!(actual.eq_unspanned(expected.as_ref()));
                }
                assert_eq!(
                    adapter.look_ahead_tree_token(n).map(|t| t.kind),
                    indexed.look_ahead_tree_token(n).map(|t| t.kind)
                );
            }
            let expected = trees.next_and_bump();
            let actual = indexed.next_and_bump();
            assert_eq!((actual.0.kind, actual.1), (expected.0.kind, expected.1));
            assert_eq!(adapter.next_and_bump(), actual);
            if actual.0.kind == token::Eof {
                break;
            }
        }
    });
}

#[test]
fn r24_tape_long_tokens_and_admission_fallback() {
    crate::rustc_span::create_default_session_globals_then(|| {
        for len in [4094, 4095, 4096, 70_000] {
            let source = "x".repeat(len);
            let tape = r24_tape(&source);
            assert_eq!(tape.materialize_token(0).0.span.hi().0, 100 + len as u32);
        }
        let build = |source: &str| {
            super::lexed_tape::LexedTape::build(
                alloc::sync::Arc::new(alloc::string::String::from(source)),
                crate::rustc_span::BytePos(100),
                0,
                crate::rustc_lexer::FrontmatterAllowed::No,
                crate::rustc_span::edition::Edition::Edition2024,
            )
        };
        for source in ["'r#name", "{]", "\"\\q\"", "r#self"] {
            assert!(build(source).is_none(), "{source:?}");
        }
        // Non-ASCII identifiers lex (to their NFC symbol); the old tape refused them.
        assert!(build("é").is_some());
        let source = "// skipped\n{ /*\n*/ r#\"\n\"#; }\n";
        let tape = r24_tape(source);
        let expected: alloc::vec::Vec<u32> = source
            .bytes()
            .enumerate()
            .filter_map(|(i, b)| (b == b'\n').then_some(i as u32))
            .collect();
        assert_eq!(tape.newlines(), expected.as_slice());
    });
}

#[test]
fn r24_arena_vec_single_word_growth_and_traits() {
    use crate::rustc_ast::ArenaVec;
    assert_eq!(core::mem::size_of::<ArenaVec<u64>>(), core::mem::size_of::<usize>());
    fn traits<
        T: Clone
            + Eq
            + core::hash::Hash
            + core::fmt::Debug
            + crate::rustc_data_structures::stable_hash::StableHash
            + Send
            + Sync,
    >() {
    }
    traits::<ArenaVec<u64>>();
    // A prior test can leave the current chunk too close to its end to grow.
    // Such a move is valid. Its replacement starts a fresh chunk, so retry once
    // to exercise in-place growth with sufficient room at the active tip.
    let mut values = (0..2)
        .find_map(|_| {
            let mut candidate = ArenaVec::with_capacity(4);
            candidate.extend(0u64..4);
            let tip = candidate.as_ptr();
            candidate.push(4);
            assert_eq!(candidate.as_slice(), &[0, 1, 2, 3, 4]);
            (candidate.as_ptr() == tip).then_some(candidate)
        })
        .expect("a small active-tip buffer with chunk room must grow in place");
    let tip = values.as_ptr();
    assert_eq!(values.as_slice(), &[0, 1, 2, 3, 4]);
    let blocker = ArenaVec::from([99u64; 32]);
    values.extend(5..1024);
    assert_ne!(values.as_ptr(), tip, "a non-tip buffer must move when it grows");
    assert_eq!(blocker.as_slice(), &[99; 32]);
    assert!(values.iter().copied().eq(0..1024));
    for value in &mut values {
        *value += 1;
    }
    let cloned = values.clone();
    assert_eq!(cloned, values);
    assert_eq!(
        values.into_iter().collect::<alloc::vec::Vec<_>>(),
        (1..=1024).collect::<alloc::vec::Vec<_>>()
    );
    let from_vec = ArenaVec::from(alloc::vec![1, 2, 3]);
    let from_thin = ArenaVec::from(thin_vec::thin_vec![1, 2, 3]);
    assert_eq!(from_vec, from_thin);
    assert_eq!(alloc::format!("{from_vec:?}"), "[1, 2, 3]");
    let mut repeated = ArenaVec::from_elem(7, 3);
    let removed = repeated.splice(1..2, [8, 9]).collect::<alloc::vec::Vec<_>>();
    assert_eq!(removed, [7]);
    assert_eq!(repeated.as_slice(), &[7, 8, 9, 7]);
    #[repr(align(128))]
    struct Aligned(u8);
    let mut aligned = ArenaVec::with_capacity(1);
    aligned.push(Aligned(1));
    aligned.push(Aligned(2));
    assert_eq!(aligned.as_ptr() as usize % 128, 0);
    assert_eq!((aligned[0].0, aligned[1].0), (1, 2));
}

#[test]
fn r24_arena_vec_batched_drops_and_partial_consumption() {
    use crate::rustc_ast::ArenaVec;
    use crate::rustc_ast::ptr::with_drop_batch;
    use core::cell::Cell;
    struct Counted<'a>(&'a Cell<usize>);
    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = Cell::new(0);
    with_drop_batch(|| {
        let mut values = ArenaVec::with_capacity(1);
        for _ in 0..100 {
            values.push(Counted(&drops));
        }
        values.truncate(80);
        assert_eq!(drops.get(), 20);
        with_drop_batch(|| {
            let mut iter = values.into_iter();
            drop(iter.next());
            drop(iter.next_back());
            assert_eq!(iter.len(), 78);
            assert_eq!(drops.get(), 22);
            drop(iter);
        });
    });
    assert_eq!(drops.get(), 100);
    static ZST_DROPS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    struct Zst;
    impl Drop for Zst {
        fn drop(&mut self) {
            ZST_DROPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
    let before = ZST_DROPS.load(core::sync::atomic::Ordering::Relaxed);
    let mut zst = ArenaVec::new();
    for _ in 0..65 {
        zst.push(Zst);
    }
    assert_eq!(zst.len(), 65);
    zst.truncate(1);
    drop(zst);
    assert_eq!(ZST_DROPS.load(core::sync::atomic::Ordering::Relaxed) - before, 65);
}

#[test]
fn r24_tape_attribute_counts_preserve_independent_macro_groups() {
    crate::rustc_span::create_default_session_globals_then(|| {
        for (source, expected, nested) in [
            ("{ #[outer(#[hidden])] fn f() {} }", 1, None),
            ("{ #![inner(#[hidden])] }", 1, None),
            ("{ /// doc\n #[outer] fn f() {} }", 2, None),
            ("{ m! { #[hidden] /// doc\n item } }", 0, Some(("{ #[hidden]", 2))),
            ("{ macro_rules! m { () => { #[hidden] item } } }", 0, Some(("{ ()", 1))),
            ("{ macro m() { #[hidden] item } }", 0, Some(("{ #[hidden]", 1))),
        ] {
            let tape = r24_tape(source);
            assert_eq!(tape.groups()[0].attr_count, expected, "{source}");
            if let Some((prefix, expected)) = nested {
                let offset = source.find(prefix).unwrap() as u32 + 100;
                let group = tape
                    .groups()
                    .iter()
                    .find(|g| tape.token_span(g.open as usize).lo().0 == offset)
                    .unwrap();
                assert_eq!(group.attr_count, expected, "independent body: {source}");
            }
        }
    });
}

#[test]
fn r24_source_and_flat_cursor_detachment_adapters() {
    use super::{DelimSpacing, DelimSpan, FlatTokenBuffer, Spacing, TokenCursor};
    use crate::rustc_ast::token;
    crate::rustc_span::create_default_session_globals_then(|| {
        let tape = r24_tape("{ { nested } tail } after");
        let mut flat = FlatTokenBuffer::with_capacity(tape.token_count(), tape.groups().len());
        let mut opens = alloc::vec::Vec::new();
        for i in 0..tape.token_count() - 1 {
            let (token, spacing) = tape.materialize_token(i);
            if token.kind.open_delim().is_some() {
                opens.push((token.span, spacing));
                flat.open(token);
            } else if token.kind.close_delim().is_some() {
                let (open, open_spacing) = opens.pop().unwrap();
                flat.close(
                    DelimSpan::from_pair(open, token.span),
                    DelimSpacing::new(open_spacing, spacing),
                );
            } else {
                flat.push(token, spacing);
            }
        }
        for mut cursor in [TokenCursor::from_lexed_tape(tape), TokenCursor::from_flat(flat)] {
            assert!(cursor.can_parallel_parse());
            assert_eq!(cursor.next_and_bump().0.kind, token::OpenBrace);
            let info = cursor.enclosing_group_info().unwrap();
            let mut detached = cursor.detached_for_group(info.group_idx).unwrap();
            assert_eq!(detached.look_ahead_tree_token(1).unwrap().kind, token::OpenBrace);
            assert!(matches!(detached.look_ahead_tree_token(2).unwrap().kind, token::Ident(..)));
            assert!(detached.look_ahead_tree_token(3).is_none());
            detached.bump_to_end();
            assert_eq!(detached.next_and_bump().0.kind, token::CloseBrace);
            assert_eq!(
                detached.next_and_bump(),
                (token::Token::new(token::Eof, info.close), Spacing::Alone)
            );
            assert!(detached.look_ahead_past_close_delim().is_none());
            cursor.bump_to_end();
            assert_eq!(cursor.next_and_bump().0.kind, token::CloseBrace);
            assert!(matches!(cursor.next_and_bump().0.kind, token::Ident(..)));
        }
    });
}

#[test]
fn w4_tape_deduplicates_repeated_unknown_symbols_and_glues_punct() {
    crate::rustc_span::create_default_session_globals_then(|| {
        let source =
            "custom_ident_alpha + custom_ident_alpha == custom_ident_beta -> custom_ident_alpha";
        let tape = r24_tape(source);
        let owner = alloc::sync::Arc::new(alloc::string::String::from(source));
        let eager = crate::rustc_parse::lexer::lex_token_trees(
            &crate::rustc_session::parse::ParseSess::new(),
            source,
            Some(&owner),
            crate::rustc_span::BytePos(100),
            None,
            crate::rustc_parse::lexer::StripTokens::Nothing,
        )
        .unwrap();
        let eager_tokens: alloc::vec::Vec<_> = eager
            .iter()
            .map(|tree| match tree {
                super::TokenTree::Token(token, spacing) => (*token, *spacing),
                _ => panic!("fixture must stay flat"),
            })
            .collect();
        let tape_tokens: alloc::vec::Vec<_> = (0..tape.token_count())
            .filter(|&i| tape.kind(i) != frontend_lex::Kind::Eof)
            .map(|i| tape.materialize_token(i))
            .collect();
        assert_eq!(tape_tokens, eager_tokens);
        let alpha = crate::rustc_span::Symbol::intern("custom_ident_alpha");
        let beta = crate::rustc_span::Symbol::intern("custom_ident_beta");
        let alpha_symbols: alloc::vec::Vec<_> = tape_tokens
            .iter()
            .filter_map(|(token, _)| match token.kind {
                crate::rustc_ast::token::Ident(symbol, _) => Some(symbol),
                _ => None,
            })
            .collect();
        assert_eq!(alpha_symbols, [alpha, alpha, beta, alpha]);
        assert_eq!(core::mem::size_of::<frontend_lex::Token>(), 16);
    });
}

fn compact_delimiter_groups_pack_twenty_bits() {
    use super::compact_token::CompactToken;
    use super::source_token::SourceKind;
    for group in [0, 4094, 4095, 65535, (1 << 20) - 2, (1 << 20) - 1, u32::MAX] {
        for spacing in [super::Spacing::Alone, super::Spacing::Joint, super::Spacing::JointHidden] {
            let token = CompactToken::with_group(7, SourceKind::OpenBrace, spacing, group);
            assert!(token.has_group());
            assert_eq!(
                token.inline_group(),
                (group < CompactToken::GROUP_OVERFLOW).then_some(group)
            );
            assert_eq!(token.spacing(), spacing);
        }
    }
}

fn short_symbol_cache_keys_preserve_exact_source_bytes() {
    use super::source_token::short_key;
    for text in
        ["a", "ab", "abc", "abcd", "abcde", "abcdef", "abcdefg", "abcdefgh", "é", "éé", "abcé"]
    {
        let bytes = short_key(text).unwrap().to_le_bytes();
        assert_eq!(&bytes[..text.len()], text.as_bytes());
        assert!(bytes[text.len()..].iter().all(|&byte| byte == 0));
    }
    for text in ["", "abcdefghi", "\0", "a\0", "\0a", "a\0b"] {
        assert_eq!(short_key(text), None, "{text:?}");
    }
}

mod arena_bulk_moves {
    use crate::rustc_ast::arena_vec::ArenaVec;
    use alloc::rc::Rc;
    use alloc::vec::Vec;
    use core::cell::Cell;

    struct Dropped(Rc<Cell<usize>>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    pub(super) fn bulk_moves_preserve_ownership_and_scratch_prefix() {
        let drops = Rc::new(Cell::new(0));
        let make = || Dropped(drops.clone());
        let mut scratch = alloc::vec![make(), make(), make()];
        let tail = ArenaVec::from_scratch(&mut scratch, 1);
        assert_eq!(scratch.len(), 1);
        assert_eq!(tail.len(), 2);
        assert_eq!(drops.get(), 0);
        drop(tail);
        assert_eq!(drops.get(), 2);
        scratch.push(make());
        drop(ArenaVec::from(scratch));
        assert_eq!(drops.get(), 4);
        drop(ArenaVec::from([make(), make()]));
        let mut inline = smallvec::SmallVec::<[_; 4]>::new();
        inline.push(make());
        drop(ArenaVec::from(inline));
        let mut spilled = smallvec::SmallVec::<[_; 1]>::new();
        spilled.extend([make(), make()]);
        drop(ArenaVec::from(spilled));
        let mut thin = thin_vec::ThinVec::new();
        thin.push(make());
        drop(ArenaVec::from(thin));
        assert_eq!(drops.get(), 10);
        assert!(ArenaVec::<Dropped>::from(Vec::new()).is_empty());
        assert!(ArenaVec::from([(); 17]).len() == 17);
    }
}
