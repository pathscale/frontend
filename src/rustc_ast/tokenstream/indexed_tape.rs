use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::Spacing;
use super::compact_token::CompactToken;
use super::source_token::{SourceKind, SourceToken, short_key, short_slot};
use crate::rustc_ast::ast::AttrStyle;
use crate::rustc_ast::token::{self, CommentKind, Delimiter, IdentIsRaw, Token};
use crate::rustc_ast::util::unicode::contains_text_flow_control_chars;
use crate::rustc_lexer::keyword_phf::{
    classify_lifetime_keyword_bytes, classify_preinterned_ident_bytes,
};
use crate::rustc_lexer::{
    Base, Cursor, DocStyle, FrontmatterAllowed, LiteralKind, TokenKind, TokenScanMeta,
};
use crate::rustc_span::{BytePos, DUMMY_SP, Span, Symbol, edition::Edition, sym};
use rustc_literal_escaper::{Mode, check_for_errors};

pub(crate) const NO_GROUP: u32 = u32::MAX;
const MAX_INLINE_SPAN: u32 = 0x7ffe;

/// Avoid narrowly missing capacity and paying for a doubled replacement. A
/// bounded middle sample avoids leading license comments and generated preludes.
/// It collects no symbols, line offsets or token storage. Sampling can start or
/// end inside a lexeme: only the capacity estimate uses these tokens.
fn tape_capacities(input: &str) -> (usize, usize) {
    let mut begin = input.len() / 2;
    while !input.is_char_boundary(begin) {
        begin += 1;
    }
    let mut end = (begin + 2048).min(input.len());
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    let sample = &input[begin..end];
    let mut cursor = Cursor::new(sample, FrontmatterAllowed::No);
    let mut tokens = 0;
    loop {
        cursor.skip_whitespace();
        let token = cursor.advance_token();
        match token.kind {
            TokenKind::Eof => break,
            TokenKind::Whitespace
            | TokenKind::LineComment { doc_style: None }
            | TokenKind::BlockComment { doc_style: None, .. } => {}
            _ => tokens += 1,
        }
    }
    let dense = !sample.is_empty() && tokens * 5 >= sample.len();
    let (token_divisor, group_divisor) = if dense { (4, 32) } else { (5, 48) };
    ((input.len() / token_divisor).max(16), (input.len() / group_divisor).max(8))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Group {
    pub(crate) delim: Delimiter,
    pub(crate) open_idx: u32,
    pub(crate) close_idx: u32,
    pub(crate) parent: u32,
    pub(crate) depth: u32,
    pub(crate) attr_count: u32,
}

const _: () = assert!(core::mem::size_of::<Group>() == 24);

#[derive(Debug)]
pub(crate) struct IndexedSourceTape {
    pub(crate) src: Arc<String>,
    pub(crate) base: BytePos,
    #[expect(
        dead_code,
        reason = "retains the original source start offset in indexed token tape metadata"
    )]
    pub(crate) start_offset: u32,
    pub(crate) tokens: Vec<CompactToken>,
    overflow_groups: Vec<(u32, u32)>,
    literal_suffixes: Vec<(u32, u32)>,
    short_symbols: Box<[(u64, Symbol); 512]>,
    pub(crate) overflow_lengths: Vec<(u32, u32)>,
    pub(crate) groups: Vec<Group>,
    pub(crate) depth_zero_cuts: Vec<u32>,
    pub(crate) raw_identifier_spans: Vec<Span>,
}

impl IndexedSourceTape {
    /// Admission emits no diagnostics and interns nothing until the full file
    /// has passed. A rejected file can be handed to the eager diagnostic lexer.
    pub(crate) fn build(
        src: Arc<String>,
        base: BytePos,
        begin: u32,
        frontmatter: FrontmatterAllowed,
        edition: Edition,
    ) -> Option<Arc<Self>> {
        let _stage = crate::rustc_data_structures::stage_timing::enter(
            crate::rustc_data_structures::stage_timing::Marker::Lex,
        );
        let end = u32::try_from(src.len()).ok()?;
        base.0.checked_add(end)?;
        let input = src.get(begin as usize..)?;
        let (token_capacity, group_capacity) = tape_capacities(input);
        let mut builder = Builder {
            tape: Self {
                src: src.clone(),
                base,
                start_offset: begin,
                tokens: Vec::with_capacity(token_capacity),
                overflow_groups: Vec::new(),
                literal_suffixes: Vec::new(),
                short_symbols: Box::new([(0, sym::empty); 512]),
                overflow_lengths: Vec::new(),
                groups: Vec::with_capacity(group_capacity),
                depth_zero_cuts: Vec::new(),
                raw_identifier_spans: Vec::new(),
            },
            short_candidates: Box::new([(0, 0); 512]),
            frames: alloc::vec![CountFrame::new(NO_GROUP, false)],
        };
        let mut cursor = Cursor::new(input, frontmatter);
        let mut pos = begin;
        let mut pending: Option<SourceToken> = None;
        loop {
            let skipped = cursor.skip_whitespace();
            pos = pos.checked_add(skipped)?;
            if skipped != 0
                && let Some(mut previous) = pending.take()
            {
                previous.spacing = Spacing::Alone;
                builder.push(previous)?;
            }
            if pending.as_ref().is_none_or(|prev| !prev.kind.is_punct())
                && matches!(cursor.frontmatter_allowed, FrontmatterAllowed::No)
                && matches!(
                    cursor.as_bytes().first(),
                    Some(
                        b'=' | b'<'
                            | b'>'
                            | b'!'
                            | b'+'
                            | b'-'
                            | b'*'
                            | b'/'
                            | b'%'
                            | b'^'
                            | b'&'
                            | b'|'
                            | b'.'
                            | b':'
                    )
                )
                && let Some(glued) = super::compact_token::try_glue_punct(cursor.as_bytes())
            {
                let lo = pos;
                if let Some(mut previous) = pending.take() {
                    previous.spacing =
                        if previous.hi == lo { Spacing::Joint } else { Spacing::Alone };
                    builder.push(previous)?;
                }
                cursor.bump_bytes(glued.len as usize);
                cursor.reset_pos_within_token();
                pos = pos.checked_add(u32::from(glued.len))?;
                builder.push(SourceToken {
                    lo,
                    hi: pos,
                    suffix: 0,
                    kind: glued.kind,
                    aux: 0,
                    spacing: glued.spacing,
                })?;
                continue;
            }
            let (raw, meta) = cursor.advance_token_for_tape();
            let lo = pos;
            pos = pos.checked_add(raw.len)?;
            let spelling = src.get(lo as usize..pos as usize)?;
            if meta.tracked {
                if meta.has_bidi {
                    return None;
                }
            } else {
                if contains_text_flow_control_chars(spelling) {
                    return None;
                }
            }
            match raw.kind {
                TokenKind::Whitespace
                | TokenKind::LineComment { doc_style: None }
                | TokenKind::BlockComment { doc_style: None, terminated: true } => {
                    if let Some(mut previous) = pending.take() {
                        previous.spacing = Spacing::Alone;
                        builder.push(previous)?;
                    }
                    continue;
                }
                _ => {}
            }
            let (kind, suffix, aux) = classify(raw.kind)?;
            if raw.len > MAX_INLINE_SPAN {
                return None;
            }
            admit(raw.kind, kind, suffix, aux, spelling, edition, meta)?;
            if kind == SourceKind::Eof && pos != end {
                return None;
            }
            let next = SourceToken { lo, hi: pos, suffix, kind, aux, spacing: Spacing::Alone };
            if let Some(mut previous) = pending.take() {
                if previous.hi == next.lo
                    && let Some(glued) = previous.kind.glue(next.kind)
                {
                    previous.kind = glued;
                    previous.hi = next.hi;
                    pending = Some(previous);
                    continue;
                }
                previous.spacing = if previous.hi != next.lo || kind == SourceKind::Eof {
                    Spacing::Alone
                } else if kind.is_punct() {
                    Spacing::Joint
                } else {
                    Spacing::JointHidden
                };
                builder.push(previous)?;
            }
            if kind == SourceKind::Eof {
                builder.push(next)?;
                break;
            }
            pending = Some(next);
        }
        if builder.frames.len() != 1 {
            return None;
        }
        builder.intern();
        Some(Arc::new(builder.tape))
    }

    pub(crate) fn token_count(&self) -> usize {
        self.tokens.len()
    }

    pub(crate) fn eof_span(&self) -> Span {
        DUMMY_SP
    }

    #[inline]
    pub(crate) fn token_len(&self, idx: usize) -> u32 {
        let token = self.tokens[idx];
        if token.has_group() {
            1
        } else if let Some(len) = token.inline_len() {
            len
        } else {
            let target = idx as u32;
            let pos = self
                .overflow_lengths
                .binary_search_by_key(&target, |&(i, _)| i)
                .expect("overflow token length recorded");
            self.overflow_lengths[pos].1
        }
    }

    #[inline]
    pub(crate) fn token_group(&self, idx: usize) -> u32 {
        if self.tokens[idx].has_group() {
            self.tokens[idx].inline_group().unwrap_or_else(|| {
                let pos = self
                    .overflow_groups
                    .binary_search_by_key(&(idx as u32), |&(i, _)| i)
                    .expect("overflow group recorded");
                self.overflow_groups[pos].1
            })
        } else {
            NO_GROUP
        }
    }

    pub(crate) fn token_span(&self, idx: usize) -> Span {
        let token = self.tokens[idx];
        if token.kind() == SourceKind::Eof {
            return self.eof_span();
        }
        self.span(token.lo(), token.lo() + self.token_len(idx))
    }

    pub(crate) fn materialize_token(&self, idx: usize) -> (Token, Spacing) {
        let token = self.tokens[idx];
        let kind = token.kind();
        let lo = token.lo();
        let spacing = token.spacing();
        if let Some(simple) = kind.simple_kind() {
            // Delimiters encode their group in the length bits. Every other
            // simple token has its full one-to-three-byte length inline.
            let span = if kind == SourceKind::Eof {
                self.eof_span()
            } else {
                let len = if token.has_group() {
                    1
                } else {
                    token.inline_len().expect("simple token length is inline")
                };
                self.span(lo, lo + len)
            };
            return (Token::new(simple, span), spacing);
        }
        let hi = lo + self.token_len(idx);
        let expanded = match kind {
            SourceKind::Ident => {
                let raw = if token.aux() == 1 { IdentIsRaw::Yes } else { IdentIsRaw::No };
                token::Ident(self.intern(lo + u32::from(token.aux() == 1) * 2, hi, false), raw)
            }
            SourceKind::Lifetime => token::Lifetime(self.intern(lo, hi, true), IdentIsRaw::No),
            SourceKind::LineDoc | SourceKind::BlockDoc => {
                let block = kind == SourceKind::BlockDoc;
                token::DocComment(
                    if block { CommentKind::Block } else { CommentKind::Line },
                    if token.aux() == 0 { AttrStyle::Outer } else { AttrStyle::Inner },
                    self.intern(lo + 3, hi - u32::from(block) * 2, false),
                )
            }
            _ => {
                let (kind, _, prefix, postfix) =
                    literal_layout(kind, token.aux()).expect("admitted literal has a layout");
                let end = if token.has_aux_u32() {
                    let pos = self
                        .literal_suffixes
                        .binary_search_by_key(&(idx as u32), |&(i, _)| i)
                        .expect("literal suffix recorded");
                    lo + self.literal_suffixes[pos].1
                } else {
                    hi
                };
                token::Literal(token::Lit::new(
                    kind,
                    self.intern(lo + prefix, end - postfix, false),
                    (end < hi).then(|| self.intern(end, hi, false)),
                ))
            }
        };
        (Token::new(expanded, self.span(lo, hi)), spacing)
    }

    pub(crate) fn group(&self, group_idx: u32) -> Option<&Group> {
        self.groups.get(group_idx as usize)
    }

    pub(crate) fn depth_zero_cuts(&self) -> &[u32] {
        &self.depth_zero_cuts
    }

    /// Line offsets are only materialized for explicit consumers, never during parsing.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "retains explicit line offset access for tape consumers")
    )]
    pub(crate) fn newlines(&self) -> Vec<u32> {
        memchr::memchr_iter(b'\n', self.src.as_bytes()).map(|i| i as u32).collect()
    }

    #[inline]
    fn intern(&self, lo: u32, hi: u32, lifetime: bool) -> Symbol {
        let text = &self.src[lo as usize..hi as usize];
        if text.len() <= 8 {
            if let Some(key) = short_key(text) {
                let (cached, symbol) = self.short_symbols[short_slot(key)];
                if cached == key {
                    return symbol;
                }
            }
            let predefined = if lifetime {
                classify_lifetime_keyword_bytes(text.as_bytes())
            } else {
                classify_preinterned_ident_bytes(text.as_bytes())
            };
            if let Some(symbol) = predefined {
                return symbol;
            }
        }
        // Long names use the interner's predefined and dynamic tables in one
        // hash/probe, avoiding a separate 128-bit packing pass on every miss.
        Symbol::intern_from_source(text, &self.src)
    }

    /// Preserve the eager lexer's first identifier occurrence order, including
    /// identifiers inside token trees the parser may never consume.
    pub(crate) fn record_symbol_gallery(
        &self,
        gallery: &crate::rustc_session::parse::SymbolGallery,
    ) {
        gallery.insert_all(
            self.tokens
                .iter()
                .enumerate()
                .filter(|(_, token)| token.kind() == SourceKind::Ident)
                .map(|(idx, token)| {
                    let lo = token.lo() + u32::from(token.aux() == 1) * 2;
                    (self.intern(lo, token.lo() + self.token_len(idx), false), self.token_span(idx))
                }),
        );
    }

    #[cfg(feature = "parallel")]
    pub(crate) fn prepare_parallel_symbols(&self) {
        // Speculative item/body transactions must not insert shared identities.
        // Only a caller attempting parallel bodies pays this pass; serial tape
        // admission and materialization retain lazy interning and no side arrays.
        for (idx, token) in self.tokens.iter().enumerate() {
            if token.kind().needs_symbol_lookup() || token.kind().is_literal() {
                self.materialize_token(idx);
            }
        }
    }

    fn span(&self, lo: u32, hi: u32) -> Span {
        // Build checked base + source length, and every non-EOF token is short
        // enough for Span's inline representation. This never interns a span.
        Span::with_root_ctxt(self.base + BytePos(lo), self.base + BytePos(hi))
    }
}

#[derive(Clone, Copy)]
struct Leaf {
    kind: SourceKind,
    macro_rules: bool,
    decl_macro: bool,
}

struct CountFrame {
    group: u32,
    attributes: u32,
    // Immediate preceding token trees, newest first. A group is not a leaf.
    previous: [Option<Leaf>; 3],
    contributes: bool,
}

impl CountFrame {
    fn new(group: u32, contributes: bool) -> Self {
        Self { group, attributes: 0, previous: [None; 3], contributes }
    }

    fn remember(&mut self, leaf: Option<Leaf>) {
        self.previous = [leaf, self.previous[0], self.previous[1]];
    }
}

struct Builder {
    tape: IndexedSourceTape,
    short_candidates: Box<[(u64, u32); 512]>,
    frames: Vec<CountFrame>,
}

impl Builder {
    fn push(&mut self, token: SourceToken) -> Option<()> {
        let index = u32::try_from(self.tape.tokens.len()).ok()?;
        if index == u32::MAX {
            return None;
        }
        let kind = token.kind;
        let length = token.hi.checked_sub(token.lo)?;
        if length > MAX_INLINE_SPAN {
            return None;
        }
        let parent = self.frames.last()?.group;
        let mut group_id = NO_GROUP;
        if let Some(delim) = kind.is_open_delim() {
            group_id = u32::try_from(self.tape.groups.len()).ok()?;
            if group_id == NO_GROUP {
                return None;
            }
            let depth = if parent == NO_GROUP {
                1
            } else {
                self.tape.groups[parent as usize].depth.checked_add(1)?
            };
            let frame = self.frames.last_mut()?;
            let [prev, prev2, prev3] = frame.previous;
            let is = |leaf: Option<Leaf>, kind| leaf.is_some_and(|leaf| leaf.kind == kind);
            let attribute = delim == Delimiter::Bracket
                && (is(prev, SourceKind::Pound)
                    || is(prev, SourceKind::Bang) && is(prev2, SourceKind::Pound));
            let invocation = is(prev, SourceKind::Bang);
            let macro_rules =
                is(prev2, SourceKind::Bang) && prev3.is_some_and(|leaf| leaf.macro_rules);
            let decl_macro = prev2.is_some_and(|leaf| leaf.decl_macro)
                || prev3.is_some_and(|leaf| leaf.decl_macro);
            frame.attributes = frame.attributes.checked_add(u32::from(attribute))?;
            frame.remember(None);
            self.frames.push(CountFrame::new(
                group_id,
                !attribute && !invocation && !macro_rules && !decl_macro,
            ));
            self.tape.groups.push(Group {
                delim,
                open_idx: index,
                close_idx: 0,
                parent,
                depth,
                attr_count: 0,
            });
        } else if let Some(delim) = kind.is_close_delim() {
            if parent == NO_GROUP {
                return None;
            }
            let frame = self.frames.pop()?;
            let group = self.tape.groups.get_mut(parent as usize)?;
            if group.delim != delim {
                return None;
            }
            group_id = parent;
            group.close_idx = index;
            group.attr_count = frame.attributes;
            if frame.contributes {
                let outer = self.frames.last_mut()?;
                outer.attributes = outer.attributes.checked_add(frame.attributes)?;
            }
            if group.parent == NO_GROUP && delim == Delimiter::Brace {
                self.tape.depth_zero_cuts.push(token.hi);
            }
        } else if kind != SourceKind::Eof {
            let frame = self.frames.last_mut()?;
            if matches!(kind, SourceKind::LineDoc | SourceKind::BlockDoc) {
                frame.attributes = frame.attributes.checked_add(1)?;
            }
            let name = if kind == SourceKind::Ident {
                let lo = token.lo + if token.aux == 1 { 2 } else { 0 };
                &self.tape.src[lo as usize..token.hi as usize]
            } else {
                ""
            };
            frame.remember(Some(Leaf {
                kind,
                macro_rules: name == "macro_rules",
                decl_macro: token.aux == 0 && name == "macro",
            }));
            if parent == NO_GROUP && kind == SourceKind::Semi {
                self.tape.depth_zero_cuts.push(token.hi);
            }
        }

        if kind == SourceKind::Ident && token.aux == 1 {
            self.tape.raw_identifier_spans.push(self.tape.span(token.lo, token.hi));
        }
        if matches!(kind, SourceKind::Ident | SourceKind::Lifetime | SourceKind::Integer) {
            let lo = token.lo + u32::from(kind == SourceKind::Ident && token.aux == 1) * 2;
            let hi = if kind == SourceKind::Integer { token.lo + token.suffix } else { token.hi };
            let name = &self.tape.src[lo as usize..hi as usize];
            if let Some(key) = short_key(name) {
                self.short_candidates[short_slot(key)] = (key, lo);
            }
        }
        let has_suffix = token.suffix < length && kind.is_literal();
        if has_suffix {
            self.tape.literal_suffixes.push((index, token.suffix));
        }
        let compact = if group_id != NO_GROUP {
            if group_id >= CompactToken::GROUP_OVERFLOW {
                self.tape.overflow_groups.push((index, group_id));
            }
            CompactToken::with_group(token.lo, kind, token.spacing, group_id)
        } else {
            CompactToken::new(token.lo, length, kind, token.spacing, token.aux, false, has_suffix)
        };
        self.tape.tokens.push(compact);
        if group_id == NO_GROUP && compact.inline_len().is_none() {
            // Tokens are appended in index order, keeping the sparse table sorted.
            self.tape.overflow_lengths.push((index, length));
        }
        Some(())
    }

    fn intern(&mut self) {
        for (slot, &(key, lo)) in self.short_candidates.iter().enumerate() {
            if key != 0 {
                let len = (64 - key.leading_zeros()).div_ceil(8) as usize;
                let text = &self.tape.src[lo as usize..lo as usize + len];
                let symbol = classify_preinterned_ident_bytes(text.as_bytes())
                    .or_else(|| classify_lifetime_keyword_bytes(text.as_bytes()))
                    .unwrap_or_else(|| Symbol::intern_from_source(text, &self.tape.src));
                self.tape.short_symbols[slot] = (key, symbol);
            }
        }
    }
}

fn admit(
    raw: TokenKind,
    kind: SourceKind,
    suffix: u32,
    aux: u8,
    spelling: &str,
    edition: Edition,
    meta: TokenScanMeta,
) -> Option<()> {
    if matches!(kind, SourceKind::Ident | SourceKind::Lifetime) {
        if !meta.tracked && !spelling.is_ascii() {
            return None;
        }
        if kind == SourceKind::Ident
            && aux == 1
            && matches!(&spelling[2..], "" | "_" | "super" | "self" | "Self" | "crate")
        {
            return None;
        }
    }
    if matches!(kind, SourceKind::LineDoc | SourceKind::BlockDoc) {
        let has_cr = if meta.tracked { meta.has_cr } else { spelling.contains('\r') };
        if has_cr {
            return None;
        }
    }
    if let TokenKind::Literal { kind: literal, .. } = raw {
        if matches!(kind, SourceKind::CStr | SourceKind::CStrRaw) && !edition.at_least_rust_2021() {
            return None;
        }
        if suffix < spelling.len() as u32 && &spelling[suffix as usize..] == "_" {
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
        let content = spelling.get(prefix as usize..suffix.checked_sub(postfix)? as usize)?;
        if let Some(mode) = mode {
            let needs_check = match mode {
                Mode::Str if meta.tracked && meta.clean_ascii_str => false,
                Mode::Str => memchr::memchr3(b'\\', b'"', b'\r', content.as_bytes()).is_some(),
                Mode::RawStr if meta.tracked => meta.has_cr,
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
    Some(())
}

// Keep the admission mapping local: unsupported tokens must fall back before
// symbols are interned, without depending on a parser or a diagnostic session.
fn classify(kind: TokenKind) -> Option<(SourceKind, u32, u8)> {
    use SourceKind as S;
    let mut suffix = 0;
    let mut aux = 0;
    let kind = match kind {
        TokenKind::Ident => S::Ident,
        TokenKind::RawIdent => {
            aux = 1;
            S::Ident
        }
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
        // Raw lifetimes, unknown prefixes, guarded strings, frontmatter,
        // invalid Unicode, unterminated comments and malformed literals.
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
