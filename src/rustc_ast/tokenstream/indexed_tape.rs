use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::compact_token::CompactToken;
use super::source_token::{SourceKind, SourceToken, token_from_compact_parts};
use super::Spacing;
use crate::rustc_ast::token::{self, Delimiter, Token};
use crate::rustc_ast::util::unicode::contains_text_flow_control_chars;
use crate::rustc_lexer::keyword_phf::{
    classify_lifetime_keyword_bytes, classify_preinterned_ident_bytes,
};
use crate::rustc_lexer::{Base, Cursor, DocStyle, FrontmatterAllowed, LiteralKind, TokenKind};
use crate::rustc_span::{BytePos, DUMMY_SP, Span, Symbol, edition::Edition, sym};
use rustc_literal_escaper::{Mode, check_for_errors};

pub(crate) const NO_GROUP: u32 = u32::MAX;
const MAX_INLINE_SPAN: u32 = 0x7ffe;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Group {
    pub(crate) delim: Delimiter,
    pub(crate) open: Span,
    pub(crate) close: Span,
    pub(crate) open_idx: u32,
    pub(crate) close_idx: u32,
    pub(crate) parent: u32,
    pub(crate) depth: u32,
    pub(crate) inside_bumps: u32,
    pub(crate) attr_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiteralMeta {
    pub(crate) kind: token::LitKind,
    pub(crate) symbol: Symbol,
    pub(crate) suffix: Option<Symbol>,
}

#[derive(Debug)]
pub(crate) struct IndexedSourceTape {
    pub(crate) src: Arc<String>,
    pub(crate) base: BytePos,
    pub(crate) start_offset: u32,
    pub(crate) tokens: Vec<CompactToken>,
    pub(crate) token_aux: Vec<u32>,
    pub(crate) token_lengths: Vec<u32>,
    pub(crate) token_groups: Vec<u32>,
    pub(crate) literals: Vec<LiteralMeta>,
    pub(crate) groups: Vec<Group>,
    pub(crate) depth_zero_cuts: Vec<u32>,
    pub(crate) newlines: Vec<u32>,
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
        let end = u32::try_from(src.len()).ok()?;
        base.0.checked_add(end)?;
        let input = src.get(begin as usize..)?;
        let mut builder = Builder {
            tape: Self {
                src: src.clone(), base, start_offset: begin,
                tokens: Vec::new(), token_aux: Vec::new(),
                token_lengths: Vec::new(), token_groups: Vec::new(),
                literals: Vec::new(), groups: Vec::new(),
                depth_zero_cuts: Vec::new(), newlines: Vec::new(),
                raw_identifier_spans: Vec::new(),
            },
            requests: Vec::new(),
            frames: alloc::vec![CountFrame::new(NO_GROUP, false)],
        };
        // Prefix bytes can contain a stripped shebang. LF offsets always refer
        // to the complete source, even though lexing begins later.
        builder.record_newlines(0, &src.as_bytes()[..begin as usize]);
        let mut cursor = Cursor::new(input, frontmatter);
        let mut pos = begin;
        let mut pending: Option<SourceToken> = None;
        loop {
            let raw = cursor.advance_token();
            let lo = pos;
            pos = pos.checked_add(raw.len)?;
            let spelling = src.get(lo as usize..pos as usize)?;
            builder.record_newlines(lo, spelling.as_bytes());
            if contains_text_flow_control_chars(spelling) {
                return None;
            }
            match raw.kind {
                TokenKind::Whitespace => continue,
                TokenKind::LineComment { doc_style: None }
                | TokenKind::BlockComment { doc_style: None, terminated: true } => continue,
                _ => {}
            }
            let (kind, suffix, aux) = classify(raw.kind)?;
            if raw.len > MAX_INLINE_SPAN {
                return None;
            }
            admit(raw.kind, kind, suffix, aux, spelling, edition)?;
            if kind == SourceKind::Eof && pos != end {
                return None;
            }
            let next = SourceToken { lo, hi: pos, suffix, kind, aux, spacing: Spacing::Alone };
            if let Some(mut previous) = pending.take() {
                if previous.hi == next.lo && let Some(glued) = previous.kind.glue(next.kind) {
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

    pub(crate) fn token_span(&self, idx: usize) -> Span {
        let token = self.tokens[idx];
        if token.kind() == SourceKind::Eof {
            return self.eof_span();
        }
        self.span(token.lo(), token.lo() + self.token_lengths[idx])
    }

    pub(crate) fn materialize_token(&self, idx: usize) -> (Token, Spacing) {
        let token = self.tokens[idx];
        let kind = token.kind();
        let symbol = kind.needs_symbol_lookup().then(|| Symbol::new(self.token_aux[idx]));
        let literal = literal_layout(kind, token.aux()).map(|_| {
            let meta = self.literals[self.token_aux[idx] as usize];
            (meta.kind, meta.symbol, meta.suffix)
        });
        token_from_compact_parts(
            kind, token.spacing(), token.aux(), self.token_span(idx), symbol, literal,
        )
    }

    pub(crate) fn group(&self, group_idx: u32) -> Option<&Group> {
        self.groups.get(group_idx as usize)
    }

    pub(crate) fn depth_zero_cuts(&self) -> &[u32] {
        &self.depth_zero_cuts
    }

    pub(crate) fn newlines(&self) -> &[u32] {
        &self.newlines
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

#[derive(Clone, Copy)]
enum Destination {
    Token(usize),
    Literal(usize),
    Suffix(usize),
}

struct SymbolRequest {
    lo: u32,
    hi: u32,
    lifetime: bool,
    destination: Destination,
}

struct Builder {
    tape: IndexedSourceTape,
    requests: Vec<SymbolRequest>,
    frames: Vec<CountFrame>,
}

impl Builder {
    fn record_newlines(&mut self, lo: u32, bytes: &[u8]) {
        self.tape.newlines.extend(memchr::memchr_iter(b'\n', bytes).map(|i| lo + i as u32));
    }

    fn request(&mut self, lo: u32, hi: u32, lifetime: bool, destination: Destination) {
        self.requests.push(SymbolRequest { lo, hi, lifetime, destination });
    }

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
            let depth = if parent == NO_GROUP { 1 } else {
                self.tape.groups[parent as usize].depth.checked_add(1)?
            };
            let frame = self.frames.last_mut()?;
            let [prev, prev2, prev3] = frame.previous;
            let is = |leaf: Option<Leaf>, kind| leaf.is_some_and(|leaf| leaf.kind == kind);
            let attribute = delim == Delimiter::Bracket
                && (is(prev, SourceKind::Pound)
                    || is(prev, SourceKind::Bang) && is(prev2, SourceKind::Pound));
            let invocation = is(prev, SourceKind::Bang);
            let macro_rules = is(prev2, SourceKind::Bang)
                && prev3.is_some_and(|leaf| leaf.macro_rules);
            let decl_macro = prev2.is_some_and(|leaf| leaf.decl_macro)
                || prev3.is_some_and(|leaf| leaf.decl_macro);
            frame.attributes = frame.attributes.checked_add(u32::from(attribute))?;
            frame.remember(None);
            self.frames.push(CountFrame::new(
                group_id, !attribute && !invocation && !macro_rules && !decl_macro,
            ));
            self.tape.groups.push(Group {
                delim, open: self.tape.span(token.lo, token.hi), close: DUMMY_SP,
                open_idx: index, close_idx: 0, parent, depth,
                inside_bumps: 0, attr_count: 0,
            });
        } else if let Some(delim) = kind.is_close_delim() {
            if parent == NO_GROUP {
                return None;
            }
            let frame = self.frames.pop()?;
            let close = self.tape.span(token.lo, token.hi);
            let group = self.tape.groups.get_mut(parent as usize)?;
            if group.delim != delim {
                return None;
            }
            group_id = parent;
            group.close = close;
            group.close_idx = index;
            group.inside_bumps = index.checked_sub(group.open_idx)?.checked_sub(1)?;
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
            } else { "" };
            frame.remember(Some(Leaf {
                kind,
                macro_rules: name == "macro_rules",
                decl_macro: token.aux == 0 && name == "macro",
            }));
            if parent == NO_GROUP && kind == SourceKind::Semi {
                self.tape.depth_zero_cuts.push(token.hi);
            }
        }

        let has_aux = kind.needs_symbol_lookup() || literal_layout(kind, token.aux).is_some();
        let mut payload = 0;
        match kind {
            SourceKind::Ident => {
                let lo = token.lo + if token.aux == 1 { 2 } else { 0 };
                if token.aux == 1 {
                    self.tape.raw_identifier_spans.push(self.tape.span(token.lo, token.hi));
                }
                self.request(lo, token.hi, false, Destination::Token(index as usize));
            }
            SourceKind::Lifetime => {
                self.request(token.lo, token.hi, true, Destination::Token(index as usize));
            }
            SourceKind::LineDoc | SourceKind::BlockDoc => {
                let postfix = u32::from(kind == SourceKind::BlockDoc) * 2;
                self.request(
                    token.lo + 3, token.hi - postfix, false, Destination::Token(index as usize),
                );
            }
            _ => {
                if let Some((kind, _, prefix, postfix)) = literal_layout(kind, token.aux) {
                    payload = u32::try_from(self.tape.literals.len()).ok()?;
                    let literal = payload as usize;
                    let end = token.lo.checked_add(token.suffix)?;
                    self.tape.literals.push(LiteralMeta {
                        kind, symbol: sym::empty, suffix: (end < token.hi).then_some(sym::empty),
                    });
                    self.request(
                        token.lo + prefix, end - postfix, false, Destination::Literal(literal),
                    );
                    if end < token.hi {
                        self.request(end, token.hi, false, Destination::Suffix(literal));
                    }
                }
            }
        }
        self.tape.tokens.push(CompactToken::new(
            token.lo, length, kind, token.spacing, token.aux, group_id != NO_GROUP, has_aux,
        ));
        self.tape.token_aux.push(payload);
        self.tape.token_lengths.push(length);
        self.tape.token_groups.push(group_id);
        Some(())
    }

    fn intern(&mut self) {
        let mut names = Vec::new();
        let mut missing = Vec::new();
        let mut resolved = Vec::with_capacity(self.requests.len());
        for (index, request) in self.requests.iter().enumerate() {
            let text = &self.tape.src[request.lo as usize..request.hi as usize];
            let predefined = if request.lifetime {
                classify_lifetime_keyword_bytes(text.as_bytes())
            } else {
                classify_preinterned_ident_bytes(text.as_bytes())
            };
            resolved.push(predefined.unwrap_or(sym::empty));
            if predefined.is_none() {
                names.push(text);
                missing.push(index);
            }
        }
        let mut symbols = alloc::vec![sym::empty; names.len()];
        Symbol::intern_batch_from_source(&names, &mut symbols);
        for (index, symbol) in missing.into_iter().zip(symbols) {
            resolved[index] = symbol;
        }
        for (request, symbol) in self.requests.iter().zip(resolved) {
            match request.destination {
                Destination::Token(index) => self.tape.token_aux[index] = symbol.as_u32(),
                Destination::Literal(index) => self.tape.literals[index].symbol = symbol,
                Destination::Suffix(index) => self.tape.literals[index].suffix = Some(symbol),
            }
        }
    }
}

fn admit(
    raw: TokenKind, kind: SourceKind, suffix: u32, aux: u8,
    spelling: &str, edition: Edition,
) -> Option<()> {
    if matches!(kind, SourceKind::Ident | SourceKind::Lifetime) {
        if !spelling.is_ascii() {
            return None;
        }
        if kind == SourceKind::Ident && aux == 1
            && matches!(&spelling[2..], "" | "_" | "super" | "self" | "Self" | "crate")
        {
            return None;
        }
    }
    if matches!(kind, SourceKind::LineDoc | SourceKind::BlockDoc) && spelling.contains('\r') {
        return None;
    }
    if let TokenKind::Literal { kind: literal, .. } = raw {
        if matches!(kind, SourceKind::CStr | SourceKind::CStrRaw) && !edition.at_least_rust_2021() {
            return None;
        }
        if suffix < spelling.len() as u32 && &spelling[suffix as usize..] == "_" {
            return None;
        }
        if let LiteralKind::Int { base: base @ (Base::Binary | Base::Octal), .. } = literal {
            if spelling[2..suffix as usize].chars()
                .any(|c| c != '_' && c.to_digit(base as u32).is_none())
            {
                return None;
            }
        }
        let (_, mode, prefix, postfix) = literal_layout(kind, aux)?;
        let content = spelling.get(prefix as usize..suffix.checked_sub(postfix)? as usize)?;
        if let Some(mode) = mode {
            let needs_check = match mode {
                Mode::Str => memchr::memchr3(b'\\', b'"', b'\r', content.as_bytes()).is_some(),
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
        TokenKind::RawIdent => { aux = 1; S::Ident }
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
                LiteralKind::RawStr { n_hashes: Some(hashes) } => { aux = hashes; S::StrRaw }
                LiteralKind::RawByteStr { n_hashes: Some(hashes) } => {
                    aux = hashes; S::ByteStrRaw
                }
                LiteralKind::RawCStr { n_hashes: Some(hashes) } => { aux = hashes; S::CStrRaw }
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
