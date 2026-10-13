use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use super::{DelimSpacing, DelimSpan, Spacing, TokenStream, TokenTree};
use crate::rustc_ast::token::{self, Token};
use crate::rustc_span::DUMMY_SP;

const OUTER: u32 = u32::MAX;

/// Source tokens own no group handles. Delimiter metadata is read only by structural
/// operations; stepping through tokens does not inspect it.
#[derive(Clone, Copy, Debug)]
struct SourceToken {
    token: Token,
    spacing: Spacing,
    matching: u32,
    enclosing_after: u32,
}

#[derive(Debug)]
pub(crate) struct FlatTokenBuffer {
    tokens: Vec<SourceToken>,
    enclosing: u32,
}

impl Default for FlatTokenBuffer {
    fn default() -> Self {
        Self { tokens: Vec::new(), enclosing: OUTER }
    }
}

impl FlatTokenBuffer {
    pub(crate) fn len(&self) -> usize {
        self.tokens.len()
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        self.tokens.truncate(len);
        self.enclosing = self.tokens.last().map_or(OUTER, |entry| entry.enclosing_after);
    }

    #[inline]
    pub(crate) fn push(&mut self, token: Token, spacing: Spacing) {
        self.tokens.push(SourceToken {
            token,
            spacing,
            matching: OUTER,
            enclosing_after: self.enclosing,
        });
    }

    pub(crate) fn open(&mut self, token: Token) {
        let index = u32::try_from(self.tokens.len()).expect("source token buffer too large");
        assert_ne!(index, OUTER);
        self.enclosing = index;
        self.push(token, Spacing::Alone);
    }

    pub(crate) fn close(&mut self, span: DelimSpan, spacing: DelimSpacing) {
        // Keep the opening delimiter's kind when recovering a mismatched close,
        // just as the tree sink's Delimited node does.
        let open = self.enclosing as usize;
        let close = u32::try_from(self.tokens.len()).expect("source token buffer too large");
        assert_ne!(close, OUTER);
        let parent = open.checked_sub(1).map_or(OUTER, |i| self.tokens[i].enclosing_after);
        let delim = self.tokens[open].token.kind.open_delim().unwrap();
        self.tokens[open].spacing = spacing.open;
        self.tokens[open].matching = close;
        self.enclosing = parent;
        self.tokens.push(SourceToken {
            token: Token::new(delim.as_close_token_kind(), span.close),
            spacing: spacing.close,
            matching: open as u32,
            enclosing_after: parent,
        });
    }

    pub(crate) fn first(&self, range: &Range<usize>) -> Option<Token> {
        if range.is_empty() {
            return None;
        }
        let token = self.tokens[range.start].token;
        (!token.kind.is_delim()).then_some(token)
    }

    fn tree(&self, index: usize) -> TokenTree {
        let entry = self.tokens[index];
        if let Some(delim) = entry.token.kind.open_delim() {
            let close = entry.matching as usize;
            let last = self.tokens[close];
            TokenTree::Delimited(
                DelimSpan::from_pair(entry.token.span, last.token.span),
                DelimSpacing::new(entry.spacing, last.spacing),
                delim,
                self.stream(index + 1..close),
            )
        } else {
            debug_assert!(!entry.token.kind.is_delim());
            TokenTree::Token(entry.token, entry.spacing)
        }
    }

    fn next_tree_index(&self, index: usize) -> usize {
        let entry = self.tokens[index];
        if entry.matching != OUTER {
            debug_assert!(entry.token.kind.open_delim().is_some());
            entry.matching as usize + 1
        } else {
            index + 1
        }
    }

    fn stream(&self, range: Range<usize>) -> TokenStream {
        let mut index = range.start;
        let mut remaining = 0;
        while index < range.end {
            remaining += 1;
            index = self.next_tree_index(index);
        }
        TokenStream::from_exact(SourceTrees { buffer: self, index: range.start, remaining })
    }
}

struct SourceTrees<'a> {
    buffer: &'a FlatTokenBuffer,
    index: usize,
    remaining: usize,
}

impl Iterator for SourceTrees<'_> {
    type Item = TokenTree;

    fn next(&mut self) -> Option<TokenTree> {
        if self.remaining == 0 {
            return None;
        }
        let tree = self.buffer.tree(self.index);
        self.index = self.buffer.next_tree_index(self.index);
        self.remaining -= 1;
        Some(tree)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for SourceTrees<'_> {}

#[derive(Clone, Debug)]
pub(super) struct FlatTokenCursor {
    buffer: Arc<FlatTokenBuffer>,
    next_idx: usize,
}

impl FlatTokenCursor {
    pub(super) fn new(buffer: FlatTokenBuffer) -> Self {
        debug_assert_eq!(buffer.enclosing, OUTER);
        Self { buffer: Arc::new(buffer), next_idx: 0 }
    }

    fn enclosing(&self) -> Option<usize> {
        let index = self.next_idx.checked_sub(1)?;
        let enclosing = self.buffer.tokens[index].enclosing_after;
        (enclosing != OUTER).then_some(enclosing as usize)
    }

    fn end(&self) -> usize {
        self.enclosing().map_or(self.buffer.len(), |i| self.buffer.tokens[i].matching as usize)
    }

    pub(super) fn look_ahead(&self, n: usize) -> Option<TokenTree> {
        assert_ne!(n, 0);
        let end = self.end();
        let mut index = self.next_idx;
        for _ in 1..n {
            if index >= end {
                return None;
            }
            index = self.buffer.next_tree_index(index);
        }
        (index < end).then(|| self.buffer.tree(index))
    }

    pub(super) fn look_ahead_token(&self, dist: usize) -> Token {
        if dist == 0 {
            return Token::dummy();
        }
        self.next_idx
            .checked_add(dist - 1)
            .and_then(|index| self.buffer.tokens.get(index))
            .map_or(Token::new(token::Eof, DUMMY_SP), |entry| entry.token)
    }

    pub(super) fn look_ahead_past_close_delim(&self) -> Option<TokenTree> {
        let open = self.enclosing().expect("cursor must be within a delimited sequence");
        let close = self.buffer.tokens[open].matching as usize;
        let index = close + 1;
        let entry = self.buffer.tokens.get(index)?;
        if entry.token.kind.close_delim().is_some() {
            None
        } else {
            Some(self.buffer.tree(index))
        }
    }

    pub(super) fn clone_enclosing_delim(&self) -> TokenTree {
        self.buffer.tree(self.enclosing().expect("cursor must be within a delimited sequence"))
    }

    pub(super) fn bump_to_end(&mut self) {
        self.next_idx = self.end();
    }

    pub(super) fn depth(&self) -> usize {
        let mut enclosing = self.enclosing();
        let mut depth = 0;
        while let Some(open) = enclosing {
            depth += 1;
            let close = self.buffer.tokens[open].matching as usize;
            let parent = self.buffer.tokens[close].enclosing_after;
            enclosing = (parent != OUTER).then_some(parent as usize);
        }
        depth
    }

    pub(super) fn parent_delim_and_span(&self) -> Option<(token::Delimiter, DelimSpan)> {
        let first = self.buffer.tokens[self.enclosing()?];
        let last = self.buffer.tokens[first.matching as usize];
        Some((
            first.token.kind.open_delim().unwrap(),
            DelimSpan::from_pair(first.token.span, last.token.span),
        ))
    }

    #[inline(always)]
    pub(super) fn next_and_bump(&mut self) -> (Token, Spacing) {
        if let Some(entry) = self.buffer.tokens.get(self.next_idx) {
            self.next_idx += 1;
            (entry.token, entry.spacing)
        } else {
            (Token::new(token::Eof, DUMMY_SP), Spacing::Alone)
        }
    }
}
