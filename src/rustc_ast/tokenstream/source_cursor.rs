use alloc::borrow::Cow;
use alloc::string::String;
use alloc::sync::Arc;

use super::flat::{FlatTokenBuffer, FlatTokenCursor};
use super::on_demand::OnDemandCursor;
use super::{DelimSpan, FrameSpare, Spacing, TokenStream, TokenTree, TreeTokenCursor};
use crate::rustc_ast::token::{Delimiter, Token};
use crate::rustc_lexer::FrontmatterAllowed;
use crate::rustc_span::BytePos;
use crate::rustc_span::edition::Edition;

/// Source parsers can pull directly from shared source bytes. The eager flat
/// fallback and macro tree cursor retain their existing recovery behavior.
#[derive(Clone, Debug)]
pub struct TokenCursor {
    inner: Cursor,
}

#[derive(Clone, Debug)]
enum Cursor {
    Trees(TreeTokenCursor),
    Flat(FlatTokenCursor),
    Source(OnDemandCursor),
}

impl TokenCursor {
    #[inline]
    pub fn new(stream: TokenStream) -> Self {
        Self { inner: Cursor::Trees(TreeTokenCursor::new(stream)) }
    }

    pub(crate) fn from_flat(buffer: FlatTokenBuffer) -> Self {
        Self { inner: Cursor::Flat(FlatTokenCursor::new(buffer)) }
    }

    pub(crate) fn from_source(
        source: Arc<String>,
        start: BytePos,
        begin: u32,
        frontmatter: FrontmatterAllowed,
        edition: Edition,
    ) -> Option<Self> {
        let cursor = OnDemandCursor::new(source, start, begin, frontmatter, edition)?;
        Some(Self { inner: Cursor::Source(cursor) })
    }

    #[cfg(feature = "parallel")]
    pub(crate) fn source_stream(&self) -> Option<&TokenStream> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.source_stream(),
            Cursor::Flat(_) | Cursor::Source(_) => None,
        }
    }

    /// Source token lookahead needs no tree construction, including across group boundaries.
    #[inline]
    pub(crate) fn flat_look_ahead_token(&self, dist: usize) -> Option<Token> {
        match &self.inner {
            Cursor::Flat(cursor) => Some(cursor.look_ahead_token(dist)),
            Cursor::Source(cursor) => Some(cursor.look_ahead_token(dist)),
            Cursor::Trees(_) => None,
        }
    }

    #[inline]
    pub fn look_ahead(&self, n: usize) -> Option<Cow<'_, TokenTree>> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.look_ahead(n).map(Cow::Borrowed),
            Cursor::Flat(cursor) => cursor.look_ahead(n).map(Cow::Owned),
            Cursor::Source(cursor) => cursor.look_ahead(n).map(Cow::Owned),
        }
    }

    /// Skip whole trees, returning a group's opening token without its body.
    pub(crate) fn look_ahead_tree_token(&self, n: usize) -> Option<Token> {
        if let Cursor::Source(cursor) = &self.inner {
            return cursor.look_ahead_tree_token(n);
        }
        self.look_ahead(n).map(|tree| match tree.as_ref() {
            TokenTree::Token(token, _) => *token,
            TokenTree::Delimited(span, _, delim, _) => {
                Token::new(delim.as_open_token_kind(), span.open)
            }
        })
    }

    pub(crate) fn raw_identifier_spans(&self) -> &[crate::rustc_span::Span] {
        match &self.inner {
            Cursor::Source(cursor) => cursor.raw_identifier_spans(),
            Cursor::Trees(_) | Cursor::Flat(_) => &[],
        }
    }

    pub(crate) fn raw_identifier_spans(&self) -> &[crate::rustc_span::Span] {
        match &self.inner {
            Cursor::Source(cursor) => cursor.raw_identifier_spans(),
            Cursor::Trees(_) | Cursor::Flat(_) => &[],
        }
    }

    pub fn look_ahead_token(&self, dist: usize) -> Option<Token> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.look_ahead_token(dist),
            Cursor::Flat(cursor) => Some(cursor.look_ahead_token(dist)),
            Cursor::Source(cursor) => Some(cursor.look_ahead_token(dist)),
        }
    }

    #[inline]
    pub fn look_ahead_past_close_delim(&self) -> Option<Cow<'_, TokenTree>> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.look_ahead_past_close_delim().map(Cow::Borrowed),
            Cursor::Flat(cursor) => cursor.look_ahead_past_close_delim().map(Cow::Owned),
            Cursor::Source(cursor) => cursor.look_ahead_past_close_delim().map(Cow::Owned),
        }
    }

    #[inline]
    pub fn clone_enclosing_delim(&self) -> TokenTree {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.clone_enclosing_delim(),
            Cursor::Flat(cursor) => cursor.clone_enclosing_delim(),
            Cursor::Source(cursor) => cursor.clone_enclosing_delim(),
        }
    }

    #[inline]
    pub fn bump_to_end(&mut self) {
        match &mut self.inner {
            Cursor::Trees(cursor) => cursor.bump_to_end(),
            Cursor::Flat(cursor) => cursor.bump_to_end(),
            Cursor::Source(cursor) => cursor.bump_to_end(),
        }
    }

    #[inline]
    pub fn depth(&self) -> usize {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.depth(),
            Cursor::Flat(cursor) => cursor.depth(),
            Cursor::Source(cursor) => cursor.depth(),
        }
    }

    #[inline]
    pub fn parent_delim_and_span(&self) -> Option<(Delimiter, DelimSpan)> {
        match &self.inner {
            Cursor::Trees(cursor) => cursor.parent_delim_and_span(),
            Cursor::Flat(cursor) => cursor.parent_delim_and_span(),
            Cursor::Source(cursor) => cursor.parent_delim_and_span(),
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
            Cursor::Source(cursor) => cursor.next_and_bump(),
        }
    }
}
