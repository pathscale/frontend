//! Direct, non-mutating recognition support for Source-backed grammar fast paths.
//!
//! Recognition is deliberately separated from consumption: a caller first checks a complete
//! shape and its follower with this window, then may materialize accepted tokens and advance via
//! `Parser::bump`. The latter preserves cursor group transitions, spacing, split-token reset,
//! dummy-span fallback, bump counts, previous/current tokens, and expectation clearing.
//!
//! This module does not implement grammar nodes or E0 replay by itself. Grammar entry points must
//! retain their full-parser arm and use a site-specific shadow runner before enabling a fast arm.

use frontend_lex::Kind;

use crate::rustc_ast::token::{Token, TokenKind};
use crate::rustc_ast::tokenstream::lexed_tape::LexedTape;
use crate::rustc_span::Span;

use super::Parser;

/// A borrowed view of the current source-token interval. `start` is the current parser token,
/// while `end` is the exclusive EOF bound. No token or span is materialized during recognition.
pub(super) struct RawWindow<'p> {
    tape: &'p LexedTape,
    start: usize,
    end: usize,
}

impl<'p> RawWindow<'p> {
    /// Return a window only for an unsplit, uncaptured Source cursor.
    /// Recovery/interpolation callers and other cursor implementations remain on the full parser.
    pub(super) fn from_parser(parser: &'p Parser<'_>) -> Option<Self> {
        if parser.capture_cfg || parser.break_last_token != 0 || parser.token.kind == TokenKind::Eof
        {
            return None;
        }
        let tape = parser.token_cursor.lexed_tape()?.as_ref();
        let next = parser.token_cursor.next_index()? as usize;
        let start = next.checked_sub(1)?;
        let end = parser.token_cursor.eof_index()? as usize;
        (start < end).then_some(Self { tape, start, end })
    }

    pub(super) fn kind(&self, offset: usize) -> Option<Kind> {
        let index = self.start.checked_add(offset)?;
        (index < self.end).then(|| self.tape.kind(index))
    }

    pub(super) fn span(&self, offset: usize) -> Option<Span> {
        let index = self.start.checked_add(offset)?;
        (index < self.end).then(|| self.tape.token_span(index))
    }

    /// Materialize a token only after the caller has accepted its complete shape.
    pub(super) fn token(
        &self,
        offset: usize,
    ) -> Option<(Token, crate::rustc_ast::tokenstream::Spacing)> {
        let index = self.start.checked_add(offset)?;
        (index < self.end).then(|| self.tape.materialize_token(index))
    }

    pub(super) fn is_ident(&self, offset: usize) -> bool {
        self.kind(offset) == Some(Kind::Ident)
    }

    /// Recognize `ident (:: ident)*` and return the number of source tokens consumed. The
    /// caller must still reject contextual keywords where its grammar requires an identifier and
    /// must validate its grammar-specific follower.
    pub(super) fn simple_ident_path(&self) -> Option<usize> {
        if !self.is_ident(0) {
            return None;
        }
        let mut offset = 1;
        loop {
            match self.kind(offset) {
                Some(Kind::PathSep) if self.is_ident(offset + 1) => offset += 2,
                Some(Kind::PathSep | Kind::Lt | Kind::Shl | Kind::LArrow | Kind::Bang) => {
                    return None;
                }
                _ => return Some(offset),
            }
        }
    }
}

/// Advance through `count` accepted source tokens using the parser's canonical bump operation.
/// A nonzero mask is the final routine's failed-probe mask after the final bump. `None` preserves
/// the mask produced by the normal bump sequence. Callers must provide this mask only when they
/// have established it from the corresponding full routine, including its deferred-expectation
/// behavior.
pub(super) fn advance_exact(parser: &mut Parser<'_>, count: usize, final_mask: Option<u128>) {
    for _ in 0..count {
        parser.bump();
    }
    if let Some(mask) = final_mask {
        parser.expected_token_types.insert_mask(mask);
    }
}

/// A conservative token-level check for grammar entry points that cannot safely fast-path a
/// recovered or interpolated token.
pub(super) fn is_plain_source_token(parser: &Parser<'_>) -> bool {
    !parser.capture_cfg
        && parser.break_last_token == 0
        && !matches!(
            parser.token.kind,
            TokenKind::NtIdent(..)
                | TokenKind::NtLifetime(..)
                | TokenKind::OpenInvisible(..)
                | TokenKind::CloseInvisible(..)
        )
}
