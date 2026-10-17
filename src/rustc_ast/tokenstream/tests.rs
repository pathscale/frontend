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

#[test]
fn p4_source_tokens_spans_equal_diagnostic_lexer() {
    use super::TokenCursor;
    use crate::rustc_ast::token;
    use crate::rustc_parse::lexer::{self, StripTokens};
    use crate::rustc_session::parse::ParseSess;
    use crate::rustc_span::{BytePos, edition};

    for &edition in edition::ALL_EDITIONS {
        crate::rustc_span::create_session_globals_then(edition, &[], None, || {
            // Covers every punctuation token, adjacency and whitespace/comment spacing, nested
            // delimiter kinds, normalized and raw identifiers, lifetimes, literal payloads and docs.
            let source = concat!(
                "//! inner docs\n",
                "/// outer docs\n",
                "fn r#type_name<'a>(x: e\u{301}, y: 'a) {\n",
                "  let _ = ([{()}]); (x+y)-z * q / r % s ^ t & u | v && w || !n ~m;\n",
                "  a==b != c <= d >= e < f > g << h >> i += j -= k *= l /= m %= n ^= o &= p |= q <<= r >>= s;\n",
                "  @ . .. ... ..= , ; : :: -> <- => # $ ?\n",
                "  1u8 2.0f32 'x' b'y' \"str\" b\"bytes\" c\"cstr\" r#\"raw\"# br##\"bytes\"## cr\"craw\" cr##\"craw\"##\n",
                "  3suffix 4.5suffix 'z'suffix b'a'suffix \"s\"suffix b\"s\"bsuffix c\"s\"csuffix r##\"r\"##suffix br\"r\"bsuffix cr\"r\"csuffix\n",
                "  // line comment spacing\n",
                "  /* block comment spacing */ value\n",
                "}\n",
            );
            // C string literals are admitted only from edition 2021 onward.
            let source = if edition < edition::Edition::Edition2021 {
                source
                    .replace(" c\"cstr\"", "")
                    .replace(" cr\"craw\" cr##\"craw\"##", "")
                    .replace(" c\"s\"csuffix", "")
                    .replace(" cr\"r\"csuffix", "")
            } else {
                alloc::string::String::from(source)
            };
            let source = source.as_str();
            let owner = alloc::sync::Arc::new(alloc::string::String::from(source));
            let psess = ParseSess::new();

            let tape = super::lexed_tape::LexedTape::build(
                owner.clone(),
                BytePos(100),
                0,
                crate::rustc_lexer::FrontmatterAllowed::No,
                edition,
            )
            .expect("the complete fixture must be admitted by the indexed lexer");
            let eager = lexer::lex_token_trees(
                &psess,
                source,
                Some(&owner),
                BytePos(100),
                None,
                StripTokens::Nothing,
            )
            .expect("the diagnostic lexer must accept the same fixture");
            let mut parser_source = crate::rustc_parse::parser::Parser::new_with_cursor(
                &psess,
                TokenCursor::from_lexed_tape(tape.clone()),
                None,
            );
            let mut parser_trees =
                crate::rustc_parse::parser::Parser::new(&psess, eager.clone(), None);
            while parser_source.token.kind != token::Eof {
                assert_eq!(parser_source.token, parser_trees.token);
                parser_source.bump();
                parser_trees.bump();
            }
            // Parser::bump turns raw cursor EOF's dummy span into the previous token's span.
            assert_eq!(parser_source.token, parser_trees.token);
            parser_source.bump();
            parser_trees.bump();
            assert_eq!(parser_source.token, parser_trees.token);

            let mut source_cursor = TokenCursor::from_lexed_tape(tape.clone());
            let mut tree_cursor = TokenCursor::new(eager);
            let mut token_index = 0usize;

            loop {
                for distance in [1, 2, 8, usize::MAX] {
                    let source_token = source_cursor.look_ahead_token(distance).unwrap();
                    // The tree oracle walks one step per requested distance, even past EOF.
                    // Clamp its walk after the whole tape while testing the source overflow path.
                    let tree_token =
                        tree_cursor.look_ahead_token(distance.min(tape.token_count() + 1)).unwrap();
                    assert_eq!(source_token, tree_token, "lookahead {distance} at {token_index}");
                }
                let source_snapshot = source_cursor.clone();
                let tree_snapshot = tree_cursor.clone();
                let actual = source_cursor.next_and_bump();
                let expected = tree_cursor.next_and_bump();
                assert_eq!(actual.0.kind, expected.0.kind, "kind at {token_index}");
                assert_eq!(actual.1, expected.1, "spacing at {token_index}");
                assert_eq!(actual.0.span, expected.0.span, "encoded span at {token_index}");
                assert_eq!(actual.0.span.lo(), expected.0.span.lo());
                assert_eq!(actual.0.span.hi(), expected.0.span.hi());
                assert_eq!(actual.0.span.ctxt(), expected.0.span.ctxt());
                assert_eq!(actual.0.span.parent(), expected.0.span.parent());

                let mut replay_source = source_snapshot;
                let mut replay_tree = tree_snapshot;
                assert_eq!(replay_source.next_and_bump(), actual);
                assert_eq!(replay_tree.next_and_bump(), expected);

                if actual.0.kind == token::Eof {
                    // Raw cursors return their EOF directly with the full-file dummy span.
                    assert_eq!(actual.0.span, DUMMY_SP);
                    assert_eq!(expected.0.span, DUMMY_SP);
                    assert_eq!(source_cursor.next_and_bump().0.span, DUMMY_SP);
                    assert_eq!(tree_cursor.next_and_bump().0.span, DUMMY_SP);
                    break;
                }
                token_index += 1;
            }

            // Each admitted group can be detached at its opening; it returns that group's
            // close once, then repeats EOF at the same close span without exposing its parent.
            for group_index in 0..tape.groups().len() {
                let group = tape.groups()[group_index];
                let mut cursor = TokenCursor::from_lexed_tape(tape.clone());
                for _ in 0..=group.open {
                    cursor.next_and_bump();
                }
                let mut detached = cursor.detached_for_group(group_index as u32).unwrap();
                assert_eq!(detached.depth(), group.depth as usize);
                assert!(detached.parent_delim_and_span().is_some());
                detached.bump_to_end();
                let close = detached.next_and_bump().0;
                assert_eq!(
                    close.kind,
                    match group.delim {
                        frontend_lex::Delimiter::Parenthesis => token::CloseParen,
                        frontend_lex::Delimiter::Brace => token::CloseBrace,
                        frontend_lex::Delimiter::Bracket => token::CloseBracket,
                    }
                );
                assert_eq!(close.span, tape.token_span(group.close as usize));
                assert_eq!(detached.depth(), 0);
                assert!(detached.parent_delim_and_span().is_none());
                for _ in 0..2 {
                    let eof = detached.next_and_bump().0;
                    assert_eq!(eof.kind, token::Eof);
                    assert_eq!(eof.span, close.span);
                }
            }
        });
    }

    crate::rustc_span::create_default_session_globals_then(|| {
        let empty = super::lexed_tape::LexedTape::build(
            alloc::sync::Arc::new(alloc::string::String::new()),
            BytePos(100),
            0,
            crate::rustc_lexer::FrontmatterAllowed::No,
            edition::Edition::Edition2024,
        )
        .expect("empty source must be admitted");
        assert_eq!(TokenCursor::from_lexed_tape(empty).next_and_bump().0.span, DUMMY_SP);

        let shebang = alloc::sync::Arc::new(alloc::string::String::from(
            "#!/usr/bin/env rustx\nfn main() {}",
        ));
        let shebang_begin = crate::rustc_lexer::strip_shebang(&shebang).unwrap() as u32;
        assert_eq!(frontend_lex::shebang_len(&shebang), Some(shebang_begin as usize));
        super::lexed_tape::LexedTape::build(
            shebang,
            BytePos(100),
            shebang_begin,
            crate::rustc_lexer::FrontmatterAllowed::No,
            edition::Edition::Edition2024,
        )
        .expect("shebang source must be admitted after its prefix");

        let long_literal = alloc::format!("\"{}\"", "x".repeat(0x8000));
        let long_tape = super::lexed_tape::LexedTape::build(
            alloc::sync::Arc::new(long_literal),
            BytePos(100),
            0,
            crate::rustc_lexer::FrontmatterAllowed::No,
            edition::Edition::Edition2024,
        )
        .expect("long literal must be admitted");
        let long_span = long_tape.materialize_token(0).0.span;
        assert!(long_span.hi().0 - long_span.lo().0 > 0x7ffe);
        assert_ne!(long_span, DUMMY_SP);
    });
}

#[test]
fn s3_range_cursor_reads_a_depth_zero_segment_then_eof() {
    use super::TokenCursor;
    use crate::rustc_ast::token;

    crate::rustc_span::create_default_session_globals_then(|| {
        let tape = r24_tape("head; { nested } tail;");
        let mut original = TokenCursor::from_lexed_tape(tape.clone());
        assert!(original.at_file_depth_zero());
        assert_eq!(
            original.next_and_bump().0.kind,
            token::Ident(
                crate::rustc_span::Symbol::intern("head"),
                crate::rustc_ast::token::IdentIsRaw::No,
            )
        );
        assert_eq!(original.next_and_bump().0.kind, token::Semi);
        assert_eq!(original.next_index(), Some(2));
        assert!(original.at_file_depth_zero());

        let mut segment = original.detached_for_range(1, 5).expect("balanced item segment");
        assert_eq!(segment.eof_index(), Some(5));
        assert!(!segment.jump_to(0));
        assert_eq!(segment.next_and_bump().0.kind, token::OpenBrace);
        assert_eq!(segment.depth(), 1);
        assert!(segment.look_ahead_token(8).unwrap().kind == token::Eof);
        let close_span = tape.materialize_token(4).0.span;
        assert_eq!(
            segment.next_and_bump().0.kind,
            token::Ident(
                crate::rustc_span::Symbol::intern("nested"),
                crate::rustc_ast::token::IdentIsRaw::No,
            )
        );
        let close = segment.next_and_bump().0;
        assert_eq!(close.kind, token::CloseBrace);
        assert_eq!(close.span, close_span);
        assert_eq!(segment.depth(), 0);
        assert!(segment.at_file_depth_zero());
        for _ in 0..2 {
            let (eof, spacing) = segment.next_and_bump();
            assert_eq!(eof.kind, token::Eof);
            assert_eq!(eof.span, close_span);
            assert_eq!(spacing, super::Spacing::Alone);
        }

        let mut jump = original.clone();
        assert!(jump.jump_to(4));
        assert_eq!(jump.depth(), 1);
        assert!(!jump.at_file_depth_zero());
        assert_eq!(jump.next_and_bump().0.kind, token::CloseBrace);
        assert!(jump.at_file_depth_zero());
        assert!(!jump.jump_to(tape.eof_index() + 1));

        assert!(original.detached_for_range(1, 4).is_none());
        assert!(original.detached_for_range(2, 5).is_none());
        assert!(original.detached_for_range(1, tape.eof_index() + 1).is_none());
        assert_eq!(original.next_and_bump().0.kind, token::OpenBrace);
    });
}

#[test]
fn arena_vec_bulk_moves_preserve_ownership() {
    arena_bulk_moves::bulk_moves_preserve_ownership_and_scratch_prefix();
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
