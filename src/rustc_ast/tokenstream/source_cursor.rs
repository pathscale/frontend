use alloc::borrow::Cow;

use super::flat::{FlatTokenBuffer, FlatTokenCursor};
use super::{DelimSpan, FrameSpare, Spacing, TokenStream, TokenTree, TreeTokenCursor};
use crate::rustc_ast::token::{Delimiter, Token};

/// Source parsers step through a shared flat buffer. Parsers over macro input keep
/// the tree cursor so invisible delimiters retain their existing behavior.
#[derive(Clone, Debug)]
pub struct TokenCursor {
    inner: Cursor,
}

#[derive(Clone, Debug)]
enum Cursor {
    Trees(TreeTokenCursor),
    Flat(FlatTokenCursor),
}

impl TokenCursor {
    #[inline]
    pub fn new(stream: TokenStream) -> Self {
        Self { inner: Cursor::Trees(TreeTokenCursor::new(stream)) }
    }

    pub(crate) fn from_flat(buffer: FlatTokenBuffer) -> Self {
        Self { inner: Cursor::Flat(FlatTokenCursor::new(buffer)) }
    }

    #[cfg(feature = "parallel")]
    pub(crate) fn source_stream(&self) -> Option<&TokenStream> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.source_stream(),
            Cursor::Flat(_) => None,
        }
    }

    /// Flat token lookahead needs no tree construction, including across group boundaries.
    #[inline]
    pub(crate) fn flat_look_ahead_token(&self, dist: usize) -> Option<Token> {
        match &self.inner {
            Cursor::Flat(cursor) => Some(cursor.look_ahead_token(dist)),
            Cursor::Trees(_) => None,
        }
    }

    #[inline]
    pub fn look_ahead(&self, n: usize) -> Option<Cow<'_, TokenTree>> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.look_ahead(n).map(Cow::Borrowed),
            Cursor::Flat(cursor) => cursor.look_ahead(n).map(Cow::Owned),
        }
    }

    pub fn look_ahead_token(&self, dist: usize) -> Option<Token> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.look_ahead_token(dist),
            Cursor::Flat(cursor) => Some(cursor.look_ahead_token(dist)),
        }
    }

    #[inline]
    pub fn look_ahead_past_close_delim(&self) -> Option<Cow<'_, TokenTree>> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.look_ahead_past_close_delim().map(Cow::Borrowed),
            Cursor::Flat(cursor) => cursor.look_ahead_past_close_delim().map(Cow::Owned),
        }
    }

    #[inline]
    pub fn clone_enclosing_delim(&self) -> TokenTree {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.clone_enclosing_delim(),
            Cursor::Flat(cursor) => cursor.clone_enclosing_delim(),
        }
    }

    #[inline]
    pub fn bump_to_end(&mut self) {
        match &mut self.inner {
            Cursor::Trees(cursor) => cursor.bump_to_end(),
            Cursor::Flat(cursor) => cursor.bump_to_end(),
        }
    }

    #[inline]
    pub fn depth(&self) -> usize {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.depth(),
            Cursor::Flat(cursor) => cursor.depth(),
        }
    }

    #[inline]
    pub fn parent_delim_and_span(&self) -> Option<(Delimiter, DelimSpan)> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.parent_delim_and_span(),
            Cursor::Flat(cursor) => cursor.parent_delim_and_span(),
        }
    }

    pub fn next_and_bump(&mut self) -> (Token, Spacing) {
        self.inlined_next_and_bump_reusing(&mut FrameSpare::default())
    }

    #[inline(always)]
    pub fn inlined_next_and_bump(&mut self) -> (Token, Spacing) {
        self.inlined_next_and_bump_reusing(&mut FrameSpare::default())
    }

    #[inline(always)]
    pub fn inlined_next_and_bump_reusing(&mut self, spare: &mut FrameSpare) -> (Token, Spacing) {
        match &mut self.inner {
            Cursor::Trees(cursor) => cursor.inlined_next_and_bump_reusing(spare),
            Cursor::Flat(cursor) => cursor.next_and_bump(),
        }
    }
}
