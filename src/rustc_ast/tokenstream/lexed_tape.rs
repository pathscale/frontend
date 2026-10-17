//! The parser's view of a source file lexed by `frontend_lex`: an `Arc<LexedFile>` and the
//! file's position in the source map.
//!
//! Everything the parser asks of a file's tokens (a token, its group, a group, the top-level
//! item cuts, the raw identifiers, the identifier gallery) is read from the file's arrays. The
//! lexer interned every spelling as it went, so nothing here reads source text or interns, and
//! the file is shared by `Arc` with every cursor and every parallel body that reads it.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use frontend_lex::{Kind, LexedFile, Options};

use super::Spacing;
use super::lexed;
use crate::rustc_ast::token::{Delimiter, Token};
use crate::rustc_lexer::FrontmatterAllowed;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::{BytePos, DUMMY_SP, Span};

pub(crate) use frontend_lex::{Group, NO_GROUP};

/// A lexed source file, as the parser's cursors read it.
#[derive(Debug)]
pub(crate) struct LexedTape {
    file: Arc<LexedFile>,
    base: BytePos,
    raw_identifier_spans: Vec<Span>,
}

/// The parser's delimiter for a lexed group's.
#[inline]
pub(crate) fn delimiter(delim: frontend_lex::Delimiter) -> Delimiter {
    match delim {
        frontend_lex::Delimiter::Parenthesis => Delimiter::Parenthesis,
        frontend_lex::Delimiter::Brace => Delimiter::Brace,
        frontend_lex::Delimiter::Bracket => Delimiter::Bracket,
    }
}

/// The delimiter a token opens, if it opens one.
#[inline]
pub(crate) fn open_delim(kind: Kind) -> Option<Delimiter> {
    match kind {
        Kind::OpenParen => Some(Delimiter::Parenthesis),
        Kind::OpenBrace => Some(Delimiter::Brace),
        Kind::OpenBracket => Some(Delimiter::Bracket),
        _ => None,
    }
}

/// The delimiter a token closes, if it closes one.
#[inline]
pub(crate) fn close_delim(kind: Kind) -> Option<Delimiter> {
    match kind {
        Kind::CloseParen => Some(Delimiter::Parenthesis),
        Kind::CloseBrace => Some(Delimiter::Brace),
        Kind::CloseBracket => Some(Delimiter::Bracket),
        _ => None,
    }
}

impl LexedTape {
    /// Lexes `text`, which starts at `base` in the source map, from `begin` (past a shebang
    /// line, if any). `None` when the lexer reports an error: the caller lexes the file again
    /// with the diagnostic lexer, which reports it (and owns frontmatter, gated and linted).
    pub(crate) fn build(
        text: Arc<String>,
        base: BytePos,
        begin: u32,
        frontmatter: FrontmatterAllowed,
        edition: Edition,
    ) -> Option<Arc<Self>> {
        let options = Options {
            edition,
            strip_shebang: begin != 0,
            frontmatter: matches!(frontmatter, FrontmatterAllowed::Yes),
        };
        let file = {
            // Charge the tokenizing to the lex row, not to whichever stage called us.
            let _stage = crate::rustc_data_structures::stage_timing::enter(
                crate::rustc_data_structures::stage_timing::Marker::Lex,
            );
            crate::rustc_span::with_session_interner(|interner| {
                frontend_lex::lex(text, options, interner)
            })
        };
        if !file.is_clean() || file.begin != begin {
            return None;
        }
        Some(Arc::new(Self::from_file(file, base)))
    }

    /// The view of an already lexed `file` that starts at `base`.
    pub(crate) fn from_file(file: Arc<LexedFile>, base: BytePos) -> Self {
        let raw_identifier_spans = file
            .raw_idents
            .iter()
            .map(|&index| {
                let t = &file.tokens[index as usize];
                lexed::span(base, t.lo, t.hi)
            })
            .collect();
        LexedTape { file, base, raw_identifier_spans }
    }

    #[cfg(test)]
    pub(crate) fn token_count(&self) -> usize {
        self.file.tokens.len()
    }

    #[inline]
    pub(crate) fn tokens(&self) -> &[frontend_lex::Token] {
        &self.file.tokens
    }

    #[inline]
    pub(crate) fn eof_index(&self) -> u32 {
        u32::try_from(self.file.tokens.len() - 1).expect("tape index fits u32")
    }

    pub(crate) fn eof_span(&self) -> Span {
        DUMMY_SP
    }

    #[inline]
    pub(crate) fn kind(&self, idx: usize) -> Kind {
        self.file.tokens[idx].kind
    }

    #[inline(always)]
    pub(crate) fn token(&self, idx: usize) -> &frontend_lex::Token {
        &self.tokens()[idx]
    }

    /// The group a delimiter opens or closes, or `NO_GROUP` for any other token.
    #[inline]
    pub(crate) fn token_group(&self, idx: usize) -> u32 {
        let t = &self.file.tokens[idx];
        if t.kind.is_open_delim() || t.kind.is_close_delim() { t.data } else { NO_GROUP }
    }

    #[inline]
    pub(crate) fn group(&self, group_idx: u32) -> Option<&Group> {
        self.file.groups.get(group_idx as usize)
    }

    #[inline]
    pub(crate) fn groups(&self) -> &[Group] {
        &self.file.groups
    }

    pub(crate) fn token_span(&self, idx: usize) -> Span {
        let t = &self.file.tokens[idx];
        if t.kind == Kind::Eof { self.eof_span() } else { lexed::span(self.base, t.lo, t.hi) }
    }

    #[inline(always)]
    pub(crate) fn materialize_token(&self, idx: usize) -> (Token, Spacing) {
        let t = self.token(idx);
        self.decode_token(t, idx)
    }

    #[inline(always)]
    pub(crate) fn decode_token(&self, t: &frontend_lex::Token, idx: usize) -> (Token, Spacing) {
        lexed::token(t, self.base, &self.file.suffixes, idx as u32)
    }

    /// Offsets of every `\n` the lexer met, in order: the file's line table, if no shebang
    /// line was skipped.
    pub(crate) fn newlines(&self) -> &[u32] {
        &self.file.newlines
    }

    /// End offsets of the top-level `;` and `}` tokens: where top-level items can end.
    pub(crate) fn depth_zero_cuts(&self) -> &[u32] {
        &self.file.item_cuts
    }

    #[inline]
    #[allow(
        dead_code,
        reason = "depth-zero item task cursor API retained for the next parser stage"
    )]
    pub(crate) fn cut_tokens(&self) -> &[u32] {
        &self.file.cut_tokens
    }

    #[inline]
    #[allow(
        dead_code,
        reason = "depth-zero item task cursor API retained for the next parser stage"
    )]
    pub(crate) fn cut_attrs(&self) -> &[u32] {
        &self.file.cut_attrs
    }

    pub(crate) fn raw_identifier_spans(&self) -> &[Span] {
        &self.raw_identifier_spans
    }

    /// Each identifier symbol's first occurrence, including in token trees the parser never
    /// reaches, in order: what the diagnostic lexer records as it goes.
    pub(crate) fn record_symbol_gallery(
        &self,
        gallery: &crate::rustc_session::parse::SymbolGallery,
    ) {
        gallery.insert_all(
            self.file
                .gallery
                .iter()
                .map(|&(symbol, index)| (symbol, self.token_span(index as usize))),
        );
    }

    /// Nothing to prepare: every symbol was interned when the file was lexed, so speculative
    /// passes never insert into the interner.
    #[cfg(feature = "parallel")]
    pub(crate) fn prepare_parallel_symbols(&self) {}
}
