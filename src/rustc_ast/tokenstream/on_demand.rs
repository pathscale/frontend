// The following lexer parts connect this cursor to parser entry points.
#![allow(dead_code)]

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::ast::AttrStyle;
use crate::rustc_ast::token::{self, CommentKind, Delimiter, IdentIsRaw, Token};
use crate::rustc_ast::util::unicode::contains_text_flow_control_chars;
use crate::rustc_lexer::{Base, Cursor, DocStyle, FrontmatterAllowed, LiteralKind, TokenKind};
use crate::rustc_span::edition::Edition;
use crate::rustc_span::{BytePos, DUMMY_SP, Span, Symbol, sym};
use rustc_literal_escaper::{Mode, check_for_errors};

use super::source_token::{SourceKind, SourceToken, short_key, short_slot};
use super::{DelimSpacing, DelimSpan, Spacing, TokenStream, TokenTree};

const OUTER: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
struct Group {
    open: u32,
    close: u32,
    parent: u32,
    depth: u32,
    delim: Delimiter,
}

#[derive(Debug)]
struct Source {
    text: Arc<String>,
    start: BytePos,
    groups: Vec<Group>,
    short_symbols: Vec<(u64, Symbol)>,
}

/// Only source with no eager lexer side effects is admitted here. Failure has
/// emitted nothing; the caller can run the original lexer from the beginning.
impl Source {
    fn checked(
        text: Arc<String>,
        start: BytePos,
        begin: u32,
        frontmatter: FrontmatterAllowed,
        edition: Edition,
    ) -> Option<Arc<Self>> {
        u32::try_from(text.len()).ok()?;
        let mut cursor = Cursor::new(text.get(begin as usize..)?, frontmatter);
        let mut pos = begin;
        let mut groups: Vec<Group> = Vec::new();
        let mut enclosing = OUTER;
        let mut short_candidates = alloc::vec![(0u64, 0u32); 512];
        loop {
            pos += cursor.skip_whitespace();
            let raw = cursor.advance_token();
            let lo = pos;
            pos += raw.len;
            let spelling = &text[lo as usize..pos as usize];
            match raw.kind {
                TokenKind::Whitespace => continue,
                TokenKind::LineComment { doc_style: None }
                | TokenKind::BlockComment { doc_style: None, terminated: true } => {
                    if contains_text_flow_control_chars(spelling) {
                        return None;
                    }
                    continue;
                }
                TokenKind::Eof => break,
                _ => {}
            }
            let (kind, suffix, aux) = classify(raw.kind)?;
            if matches!(kind, SourceKind::Ident | SourceKind::Lifetime) {
                if !spelling.is_ascii() {
                    return None;
                }
                if kind == SourceKind::Ident
                    && let Some(key) = short_key(spelling)
                {
                    short_candidates[short_slot(key)] = (key, lo);
                }
            }
            if matches!(kind, SourceKind::LineDoc | SourceKind::BlockDoc)
                && (spelling.contains('\r') || contains_text_flow_control_chars(spelling))
            {
                return None;
            }
            if let TokenKind::Literal { kind: literal, .. } = raw.kind {
                if matches!(kind, SourceKind::CStr | SourceKind::CStrRaw)
                    && !edition.at_least_rust_2021()
                {
                    return None;
                }
                if suffix < raw.len && &spelling[suffix as usize..] == "_" {
                    return None;
                }
                if let LiteralKind::Int { base: base @ (Base::Binary | Base::Octal), .. } = literal
                    && spelling[2..suffix as usize]
                        .chars()
                        .any(|c| c != '_' && c.to_digit(base as u32).is_none())
                {
                    return None;
                }
                let (_, mode, prefix, postfix) = literal_layout(kind, aux)?;
                let content =
                    spelling.get(prefix as usize..suffix.checked_sub(postfix)? as usize)?;
                if contains_text_flow_control_chars(content) {
                    return None;
                }
                if let Some(mode) = mode {
                    let needs_check = match mode {
                        Mode::Str => {
                            memchr::memchr3(b'\\', b'"', b'\r', content.as_bytes()).is_some()
                        }
                        Mode::RawStr => memchr::memchr(b'\r', content.as_bytes()).is_some(),
                        _ => true,
                    };
                    if needs_check {
                        let mut clean = true;
                        check_for_errors(content, mode, |_, _| clean = false);
                        if !clean {
                            return None;
                        }
                    }
                }
            }
            if let Some(delim) = kind.open_delim() {
                let index = u32::try_from(groups.len()).ok()?;
                if index == OUTER {
                    return None;
                }
                let depth = if enclosing == OUTER {
                    1
                } else {
                    groups[enclosing as usize].depth.checked_add(1)?
                };
                groups.push(Group { open: lo, close: 0, parent: enclosing, depth, delim });
                enclosing = index;
            } else if let Some(delim) = kind.close_delim() {
                let group = groups.get_mut(enclosing as usize)?;
                if group.delim != delim {
                    return None;
                }
                group.close = lo;
                enclosing = group.parent;
            }
        }
        if enclosing != OUTER {
            return None;
        }
        // Intern only after eligibility is established. The immutable cache is
        // shared by every snapshot; misses still intern from the source owner.
        let short_symbols = short_candidates
            .into_iter()
            .map(|(key, lo)| {
                let symbol = if key == 0 {
                    sym::empty
                } else {
                    let len = (64 - key.leading_zeros()).div_ceil(8) as usize;
                    Symbol::intern_from_source(&text[lo as usize..lo as usize + len], &text)
                };
                (key, symbol)
            })
            .collect();
        Some(Arc::new(Self { text, start, groups, short_symbols }))
    }

    /// No references into the source escape a scan. Resuming from a byte
    /// offset keeps the owning cursor free of self references and lifetimes.
    fn scan(&self, offset: u32) -> SourceToken {
        let mut cursor = Cursor::new(&self.text[offset as usize..], FrontmatterAllowed::No);
        let mut pos = offset;
        let mut whitespace = false;
        loop {
            let skipped = cursor.skip_whitespace();
            pos += skipped;
            whitespace |= skipped != 0;
            let raw = cursor.advance_token();
            let lo = pos;
            pos += raw.len;
            match raw.kind {
                TokenKind::Whitespace
                | TokenKind::LineComment { doc_style: None }
                | TokenKind::BlockComment { doc_style: None, .. } => {
                    whitespace = true;
                }
                _ => {
                    let (kind, suffix, aux) =
                        classify(raw.kind).expect("preflight admitted lexeme");
                    return SourceToken {
                        lo,
                        hi: pos,
                        suffix,
                        kind,
                        aux,
                        // Before preparation this records preceding whitespace.
                        spacing: if whitespace { Spacing::Alone } else { Spacing::Joint },
                    };
                }
            }
        }
    }

    fn prepare(&self, mut token: SourceToken) -> (SourceToken, SourceToken) {
        let mut next = self.scan(token.hi);
        // Delimiters use the existing lexer's minimal, non-gluing bump.
        if token.kind.open_delim().is_none() && token.kind.close_delim().is_none() {
            while next.spacing != Spacing::Alone {
                let Some(kind) = token.kind.glue(next.kind) else { break };
                token.kind = kind;
                token.hi = next.hi;
                next = self.scan(next.hi);
            }
        }
        token.spacing = if next.spacing == Spacing::Alone || next.kind == SourceKind::Eof {
            Spacing::Alone
        } else if next.kind.is_punct() {
            Spacing::Joint
        } else {
            Spacing::JointHidden
        };
        (token, next)
    }

    fn span(&self, lo: u32, hi: u32) -> Span {
        Span::with_root_ctxt(self.start + BytePos(lo), self.start + BytePos(hi))
    }

    fn intern(&self, lo: u32, hi: u32) -> Symbol {
        Symbol::intern_from_source(&self.text[lo as usize..hi as usize], &self.text)
    }

    fn expand(&self, token: SourceToken) -> Token {
        if token.kind == SourceKind::Eof {
            return Token::new(token::Eof, DUMMY_SP);
        }
        let kind = if let Some(kind) = token.kind.simple_kind() {
            kind
        } else {
            match token.kind {
                SourceKind::Ident => {
                    let text = &self.text[token.lo as usize..token.hi as usize];
                    let symbol = short_key(text)
                        .and_then(|key| {
                            let (cached, symbol) = self.short_symbols[short_slot(key)];
                            (cached == key).then_some(symbol)
                        })
                        .unwrap_or_else(|| self.intern(token.lo, token.hi));
                    token::Ident(symbol, IdentIsRaw::No)
                }
                SourceKind::Lifetime => {
                    token::Lifetime(self.intern(token.lo, token.hi), IdentIsRaw::No)
                }
                SourceKind::LineDoc | SourceKind::BlockDoc => {
                    let block = token.kind == SourceKind::BlockDoc;
                    token::DocComment(
                        if block { CommentKind::Block } else { CommentKind::Line },
                        if token.aux == 0 { AttrStyle::Outer } else { AttrStyle::Inner },
                        self.intern(token.lo + 3, token.hi - if block { 2 } else { 0 }),
                    )
                }
                _ => {
                    let (kind, _, prefix, postfix) =
                        literal_layout(token.kind, token.aux).expect("preflight admitted literal");
                    let end = token.lo + token.suffix;
                    let symbol = self.intern(token.lo + prefix, end - postfix);
                    let suffix = (end < token.hi).then(|| self.intern(end, token.hi));
                    token::Literal(token::Lit { kind, symbol, suffix })
                }
            }
        };
        Token::new(kind, self.span(token.lo, token.hi))
    }

    fn group_at(&self, lo: u32) -> u32 {
        self.groups
            .binary_search_by_key(&lo, |group| group.open)
            .expect("preflight indexed opening delimiter") as u32
    }

    fn delim_span(&self, group: Group) -> DelimSpan {
        DelimSpan::from_pair(
            self.span(group.open, group.open + 1),
            self.span(group.close, group.close + 1),
        )
    }
}

/// No token tape or mutable session reference is retained by a snapshot.
#[derive(Clone, Debug)]
pub(super) struct OnDemandCursor {
    source: Arc<Source>,
    current: SourceToken,
    pending: SourceToken,
    enclosing: u32,
}

impl OnDemandCursor {
    pub(super) fn new(
        text: Arc<String>,
        start: BytePos,
        begin: u32,
        frontmatter: FrontmatterAllowed,
        edition: Edition,
    ) -> Option<Self> {
        let source = Source::checked(text, start, begin, frontmatter, edition)?;
        Some(Self::at(source, begin, OUTER))
    }

    fn at(source: Arc<Source>, offset: u32, enclosing: u32) -> Self {
        let (current, pending) = source.prepare(source.scan(offset));
        Self { source, current, pending, enclosing }
    }

    fn advance(&mut self) {
        if self.current.kind.open_delim().is_some() {
            self.enclosing = self.source.group_at(self.current.lo);
        } else if self.current.kind.close_delim().is_some() {
            let group = self.source.groups[self.enclosing as usize];
            debug_assert_eq!(group.close, self.current.lo);
            self.enclosing = group.parent;
        }
        (self.current, self.pending) = self.source.prepare(self.pending);
    }

    #[inline]
    pub(super) fn next_and_bump(&mut self) -> (Token, Spacing) {
        let token = self.source.expand(self.current);
        let spacing = self.current.spacing;
        self.advance();
        (token, spacing)
    }

    pub(super) fn look_ahead_token(&self, dist: usize) -> Token {
        if dist == 0 {
            return Token::dummy();
        }
        let mut cursor = self.clone();
        for _ in 1..dist {
            if cursor.current.kind == SourceKind::Eof {
                break;
            }
            cursor.advance();
        }
        cursor.source.expand(cursor.current)
    }

    fn skip_tree(&mut self) {
        if self.current.kind.open_delim().is_some() {
            let group = self.source.groups[self.source.group_at(self.current.lo) as usize];
            *self = Self::at(self.source.clone(), group.close + 1, self.enclosing);
        } else {
            self.advance();
        }
    }

    fn tree_cursor(&self, n: usize) -> Option<Self> {
        assert_ne!(n, 0);
        let mut cursor = self.clone();
        for _ in 1..n {
            if cursor.current.kind.close_delim().is_some() || cursor.current.kind == SourceKind::Eof
            {
                return None;
            }
            cursor.skip_tree();
        }
        (cursor.current.kind.close_delim().is_none() && cursor.current.kind != SourceKind::Eof)
            .then_some(cursor)
    }

    /// Structural predicates need only a token or a group's opening delimiter.
    pub(super) fn look_ahead_tree_token(&self, n: usize) -> Option<Token> {
        let cursor = self.tree_cursor(n)?;
        Some(cursor.source.expand(cursor.current))
    }

    fn take_tree(&mut self) -> TokenTree {
        if let Some(delim) = self.current.kind.open_delim() {
            let (open, open_spacing) = self.next_and_bump();
            let mut children = Vec::new();
            while self.current.kind.close_delim().is_none() && self.current.kind != SourceKind::Eof
            {
                children.push(self.take_tree());
            }
            debug_assert_eq!(self.current.kind.close_delim(), Some(delim));
            let (close, close_spacing) = self.next_and_bump();
            TokenTree::Delimited(
                DelimSpan::from_pair(open.span, close.span),
                DelimSpacing::new(open_spacing, close_spacing),
                delim,
                TokenStream::new(children),
            )
        } else {
            assert!(
                self.current.kind.close_delim().is_none() && self.current.kind != SourceKind::Eof
            );
            let (token, spacing) = self.next_and_bump();
            TokenTree::Token(token, spacing)
        }
    }

    pub(super) fn look_ahead(&self, n: usize) -> Option<TokenTree> {
        Some(self.tree_cursor(n)?.take_tree())
    }

    pub(super) fn look_ahead_past_close_delim(&self) -> Option<TokenTree> {
        let group = self.source.groups[self.enclosing as usize];
        let mut cursor = Self::at(self.source.clone(), group.close + 1, group.parent);
        if cursor.current.kind.close_delim().is_some() || cursor.current.kind == SourceKind::Eof {
            None
        } else {
            Some(cursor.take_tree())
        }
    }

    pub(super) fn clone_enclosing_delim(&self) -> TokenTree {
        let group = self.source.groups[self.enclosing as usize];
        Self::at(self.source.clone(), group.open, group.parent).take_tree()
    }

    pub(super) fn bump_to_end(&mut self) {
        let group = self.source.groups[self.enclosing as usize];
        *self = Self::at(self.source.clone(), group.close, self.enclosing);
    }

    pub(super) fn depth(&self) -> usize {
        if self.enclosing == OUTER {
            0
        } else {
            self.source.groups[self.enclosing as usize].depth as usize
        }
    }

    pub(super) fn parent_delim_and_span(&self) -> Option<(Delimiter, DelimSpan)> {
        let group = *self.source.groups.get(self.enclosing as usize)?;
        Some((group.delim, self.source.delim_span(group)))
    }
}

fn classify(kind: TokenKind) -> Option<(SourceKind, u32, u8)> {
    use SourceKind as S;
    let mut suffix = 0;
    let mut aux = 0;
    let kind = match kind {
        TokenKind::Ident => S::Ident,
        TokenKind::Lifetime { starts_with_number: false } => S::Lifetime,
        TokenKind::Semi => S::Semi,
        TokenKind::Comma => S::Comma,
        TokenKind::Dot => S::Dot,
        TokenKind::OpenParen => S::OpenParen,
        TokenKind::CloseParen => S::CloseParen,
        TokenKind::OpenBrace => S::OpenBrace,
        TokenKind::CloseBrace => S::CloseBrace,
        TokenKind::OpenBracket => S::OpenBracket,
        TokenKind::CloseBracket => S::CloseBracket,
        TokenKind::At => S::At,
        TokenKind::Pound => S::Pound,
        TokenKind::Tilde => S::Tilde,
        TokenKind::Question => S::Question,
        TokenKind::Colon => S::Colon,
        TokenKind::Dollar => S::Dollar,
        TokenKind::Eq => S::Eq,
        TokenKind::Bang => S::Bang,
        TokenKind::Lt => S::Lt,
        TokenKind::Gt => S::Gt,
        TokenKind::Minus => S::Minus,
        TokenKind::And => S::And,
        TokenKind::Or => S::Or,
        TokenKind::Plus => S::Plus,
        TokenKind::Star => S::Star,
        TokenKind::Slash => S::Slash,
        TokenKind::Caret => S::Caret,
        TokenKind::Percent => S::Percent,
        TokenKind::Eof => S::Eof,
        TokenKind::LineComment { doc_style: Some(style) }
        | TokenKind::BlockComment { doc_style: Some(style), terminated: true } => {
            aux = u8::from(matches!(style, DocStyle::Inner));
            if matches!(kind, TokenKind::LineComment { .. }) { S::LineDoc } else { S::BlockDoc }
        }
        TokenKind::Literal { kind, suffix_start } => {
            suffix = suffix_start;
            match kind {
                LiteralKind::Int { empty_int: false, .. } => S::Integer,
                LiteralKind::Float { base: Base::Decimal, empty_exponent: false } => S::Float,
                LiteralKind::Char { terminated: true } => S::Char,
                LiteralKind::Byte { terminated: true } => S::Byte,
                LiteralKind::Str { terminated: true } => S::Str,
                LiteralKind::ByteStr { terminated: true } => S::ByteStr,
                LiteralKind::CStr { terminated: true } => S::CStr,
                LiteralKind::RawStr { n_hashes: Some(hashes) } => {
                    aux = hashes;
                    S::StrRaw
                }
                LiteralKind::RawByteStr { n_hashes: Some(hashes) } => {
                    aux = hashes;
                    S::ByteStrRaw
                }
                LiteralKind::RawCStr { n_hashes: Some(hashes) } => {
                    aux = hashes;
                    S::CStrRaw
                }
                _ => return None,
            }
        }
        // This includes raw identifiers/lifetimes, unknown prefixes, guarded
        // strings, frontmatter, invalid Unicode and malformed literals.
        _ => return None,
    };
    Some((kind, suffix, aux))
}

fn literal_layout(kind: SourceKind, aux: u8) -> Option<(token::LitKind, Option<Mode>, u32, u32)> {
    use SourceKind as S;
    let hashes = u32::from(aux);
    Some(match kind {
        S::Integer => (token::Integer, None, 0, 0),
        S::Float => (token::Float, None, 0, 0),
        S::Char => (token::Char, Some(Mode::Char), 1, 1),
        S::Byte => (token::Byte, Some(Mode::Byte), 2, 1),
        S::Str => (token::Str, Some(Mode::Str), 1, 1),
        S::ByteStr => (token::ByteStr, Some(Mode::ByteStr), 2, 1),
        S::CStr => (token::CStr, Some(Mode::CStr), 2, 1),
        S::StrRaw => (token::StrRaw(aux), Some(Mode::RawStr), 2 + hashes, 1 + hashes),
        S::ByteStrRaw => (token::ByteStrRaw(aux), Some(Mode::RawByteStr), 3 + hashes, 1 + hashes),
        S::CStrRaw => (token::CStrRaw(aux), Some(Mode::RawCStr), 3 + hashes, 1 + hashes),
        _ => return None,
    })
}
