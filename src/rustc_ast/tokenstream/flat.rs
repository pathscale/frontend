use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use super::{DelimSpacing, DelimSpan, Spacing, TokenStream, TokenTree};
use crate::rustc_ast::token::{self, Token};
use crate::rustc_span::{DUMMY_SP, Span, kw};

const OUTER: u32 = u32::MAX;

/// Source tokens own no group handles. Delimiter metadata is read only by structural
/// operations; stepping through tokens does not inspect it.
#[derive(Clone, Copy, Debug)]
struct SourceToken {
    token: Token,
    spacing: Spacing,
    matching: u32,
    enclosing_after: u32,
    group: u32,
}

#[derive(Clone, Debug, Default)]
struct Counts {
    bumps: u32,
    attrs: u32,
    // Previous sibling trees, nearest first. Groups occupy a None slot.
    previous: [Option<Token>; 3],
}

impl Counts {
    fn remember(&mut self, token: Option<Token>) {
        self.previous = [token, self.previous[0], self.previous[1]];
    }
}

#[derive(Debug)]
struct Group {
    open_index: u32,
    close_index: u32,
    #[cfg_attr(
        not(any(feature = "parallel", test)),
        expect(dead_code, reason = "parent links support owned body detachment")
    )]
    parent: u32,
    depth: usize,
    counts: Counts,
    attribute: bool,
    suppress_attrs: bool,
}

#[derive(Debug)]
pub(crate) struct FlatTokenBuffer {
    tokens: Vec<SourceToken>,
    enclosing: u32,
    groups: Vec<Group>,
    root: Counts,
    only_source_tokens: bool,
}

impl Default for FlatTokenBuffer {
    fn default() -> Self {
        Self::with_capacity(0, 0)
    }
}

impl FlatTokenBuffer {
    pub(crate) fn with_capacity(token_capacity: usize, group_capacity: usize) -> Self {
        Self {
            tokens: Vec::with_capacity(token_capacity),
            enclosing: OUTER,
            groups: Vec::with_capacity(group_capacity),
            root: Counts::default(),
            only_source_tokens: true,
        }
    }

    #[cfg(any(feature = "parallel", test))]
    pub(crate) fn has_only_source_tokens(&self) -> bool {
        self.only_source_tokens && self.enclosing == OUTER
    }

    // The sink's close signature does not carry the actual closing token kind.
    // A lexer that recovers a mismatch must call this before normalizing it.
    pub(crate) fn invalidate_source_tokens(&mut self) {
        self.only_source_tokens = false;
    }

    fn valid_span(span: Span) -> bool {
        !span.is_dummy() && span.ctxt().is_root() && span.parent().is_none()
    }

    fn counts_mut(&mut self) -> &mut Counts {
        if self.enclosing == OUTER {
            &mut self.root
        } else {
            let group = self.tokens[self.enclosing as usize].group;
            &mut self.groups[group as usize].counts
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.tokens.len()
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        self.only_source_tokens = false;
        if len >= self.tokens.len() {
            return;
        }
        self.tokens.truncate(len);
        self.enclosing = self.tokens.last().map_or(OUTER, |entry| entry.enclosing_after);
        // Recovery must never make a previously rejected buffer eligible again.
        // Replaying the retained prefix restores independent group counts and
        // clears matches whose close was removed. Group IDs stay in opening order.
        let retained = core::mem::take(&mut self.tokens);
        self.enclosing = OUTER;
        self.groups.clear();
        self.root = Counts::default();
        for entry in retained {
            if entry.token.kind.open_delim().is_some() {
                self.open(entry.token);
            } else if entry.token.kind.close_delim().is_some() && self.enclosing != OUTER {
                let first = self.tokens[self.enclosing as usize];
                self.close(
                    DelimSpan::from_pair(first.token.span, entry.token.span),
                    DelimSpacing::new(first.spacing, entry.spacing),
                );
            } else {
                self.push(entry.token, entry.spacing);
            }
            self.tokens.last_mut().unwrap().spacing = entry.spacing;
        }
    }

    #[inline]
    pub(crate) fn push(&mut self, token: Token, spacing: Spacing) {
        self.only_source_tokens &= Self::valid_span(token.span)
            && !matches!(
                token.kind,
                token::NtIdent(..)
                    | token::NtLifetime(..)
                    | token::OpenInvisible(..)
                    | token::CloseInvisible(..)
            )
            && !token.kind.is_delim();
        let counts = self.counts_mut();
        counts.bumps = counts.bumps.checked_add(1).expect("source token count overflow");
        if matches!(token.kind, token::DocComment(..)) {
            counts.attrs = counts.attrs.checked_add(1).expect("source attribute count overflow");
        }
        counts.remember(Some(token));
        self.tokens.push(SourceToken {
            token,
            spacing,
            matching: OUTER,
            enclosing_after: self.enclosing,
            group: OUTER,
        });
    }

    pub(crate) fn open(&mut self, token: Token) {
        let index = u32::try_from(self.tokens.len()).expect("source token buffer too large");
        assert_ne!(index, OUTER);
        let delim = token.kind.open_delim().expect("opening delimiter required");
        self.only_source_tokens &=
            Self::valid_span(token.span) && !matches!(delim, token::Delimiter::Invisible(..));
        let [prev, prev2, prev3] = self.counts_mut().previous;
        let attribute = delim == token::Delimiter::Bracket
            && (prev.is_some_and(|t| t == token::Pound)
                || prev.is_some_and(|t| t == token::Bang)
                    && prev2.is_some_and(|t| t == token::Pound));
        let invocation = prev.is_some_and(|t| t == token::Bang);
        let macro_rules = prev2.is_some_and(|t| t == token::Bang)
            && prev3.is_some_and(|t| t.is_ident_named(kw::MacroRules));
        let decl_macro = prev2.is_some_and(|t| t.is_keyword(kw::Macro))
            || prev3.is_some_and(|t| t.is_keyword(kw::Macro));
        let parent = if self.enclosing == OUTER {
            OUTER
        } else {
            self.tokens[self.enclosing as usize].group
        };
        let depth = if parent == OUTER { 1 } else { self.groups[parent as usize].depth + 1 };
        let group = u32::try_from(self.groups.len()).expect("source group buffer too large");
        assert_ne!(group, OUTER);
        self.groups.push(Group {
            open_index: index,
            close_index: OUTER,
            parent,
            depth,
            counts: Counts::default(),
            attribute,
            suppress_attrs: attribute || invocation || macro_rules || decl_macro,
        });
        self.enclosing = index;
        self.tokens.push(SourceToken {
            token,
            spacing: Spacing::Alone,
            matching: OUTER,
            enclosing_after: index,
            group,
        });
    }

    pub(crate) fn close(&mut self, span: DelimSpan, spacing: DelimSpacing) {
        // Keep the opening delimiter's kind when recovering a mismatched close,
        // just as the tree sink's Delimited node does.
        let open = self.enclosing as usize;
        let close = u32::try_from(self.tokens.len()).expect("source token buffer too large");
        assert_ne!(close, OUTER);
        let parent = open.checked_sub(1).map_or(OUTER, |i| self.tokens[i].enclosing_after);
        let delim = self.tokens[open].token.kind.open_delim().unwrap();
        self.only_source_tokens &= Self::valid_span(span.open)
            && Self::valid_span(span.close)
            && span.open == self.tokens[open].token.span
            && span.open.hi() <= span.close.lo();
        let group = self.tokens[open].group;
        let metadata = &mut self.groups[group as usize];
        metadata.close_index = close;
        let bumps = metadata
            .counts
            .bumps
            .checked_add(if delim.skip() { 0 } else { 2 })
            .expect("source token count overflow");
        let attrs = u32::from(metadata.attribute)
            .checked_add(if metadata.suppress_attrs { 0 } else { metadata.counts.attrs })
            .expect("source attribute count overflow");
        self.tokens[open].spacing = spacing.open;
        self.tokens[open].matching = close;
        self.enclosing = parent;
        let counts = self.counts_mut();
        counts.bumps = counts.bumps.checked_add(bumps).expect("source token count overflow");
        counts.attrs = counts.attrs.checked_add(attrs).expect("source attribute count overflow");
        counts.remember(None);
        self.tokens.push(SourceToken {
            token: Token::new(delim.as_close_token_kind(), span.close),
            spacing: spacing.close,
            matching: open as u32,
            enclosing_after: parent,
            group,
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
    end_idx: usize,
    end_group: u32,
}

impl FlatTokenCursor {
    pub(super) fn new(buffer: FlatTokenBuffer) -> Self {
        debug_assert_eq!(buffer.enclosing, OUTER);
        let end_idx = buffer.len();
        Self { buffer: Arc::new(buffer), next_idx: 0, end_idx, end_group: OUTER }
    }

    fn enclosing(&self) -> Option<usize> {
        let index = self.next_idx.checked_sub(1)?;
        let enclosing = self.buffer.tokens.get(index)?.enclosing_after;
        if enclosing == OUTER {
            return None;
        }
        if self.end_group != OUTER {
            let bound = &self.buffer.groups[self.end_group as usize];
            if enclosing < bound.open_index || enclosing > bound.close_index {
                return None;
            }
        }
        Some(enclosing as usize)
    }

    fn end(&self) -> usize {
        self.enclosing()
            .map_or(self.end_idx, |i| self.end_idx.min(self.buffer.tokens[i].matching as usize))
    }

    fn eof(&self) -> Token {
        let span = if self.end_group == OUTER {
            DUMMY_SP
        } else {
            let group = &self.buffer.groups[self.end_group as usize];
            self.buffer.tokens[group.close_index as usize].token.span
        };
        Token::new(token::Eof, span)
    }

    #[cfg(any(feature = "parallel", test))]
    pub(super) fn can_parallel_parse(&self) -> bool {
        self.buffer.has_only_source_tokens()
    }

    #[cfg(any(feature = "parallel", test))]
    pub(super) fn enclosing_group_info(&self) -> Option<super::on_demand::EnclosingGroupInfo> {
        let first = self.buffer.tokens[self.enclosing()?];
        let group = &self.buffer.groups[first.group as usize];
        if group.close_index == OUTER {
            return None;
        }
        Some(super::on_demand::EnclosingGroupInfo {
            group_idx: first.group,
            delim: first.token.kind.open_delim().unwrap(),
            open: first.token.span,
            close: self.buffer.tokens[group.close_index as usize].token.span,
            inside_bumps: group.counts.bumps,
            attr_count: group.counts.attrs,
        })
    }

    #[cfg(any(feature = "parallel", test))]
    pub(super) fn detached_for_group(&self, group_idx: u32) -> Option<Self> {
        let group = self.buffer.groups.get(group_idx as usize)?;
        if group.close_index == OUTER {
            return None;
        }
        let end_idx = group.close_index as usize + 1;
        if self.next_idx <= group.open_index as usize
            || self.next_idx > end_idx
            || end_idx > self.end_idx
        {
            return None;
        }
        // Containment in the existing bound also handles an already-current close.
        if self.end_group != OUTER
            && group.open_index < self.buffer.groups[self.end_group as usize].open_index
        {
            return None;
        }
        if self.next_idx == end_idx {
            let parent_open = self.buffer.tokens[self.next_idx - 1].enclosing_after;
            let parent = if parent_open == OUTER {
                OUTER
            } else {
                self.buffer.tokens[parent_open as usize].group
            };
            if parent != group.parent {
                return None;
            }
        }
        Some(Self {
            buffer: Arc::clone(&self.buffer),
            next_idx: self.next_idx,
            end_idx,
            end_group: group_idx,
        })
    }

    fn tree_index(&self, n: usize) -> Option<usize> {
        assert_ne!(n, 0);
        let end = self.end();
        let mut index = self.next_idx;
        for _ in 1..n {
            if index >= end {
                return None;
            }
            index = self.buffer.next_tree_index(index);
        }
        let entry = (index < end).then(|| self.buffer.tokens[index])?;
        if entry.token.kind == token::Eof || entry.token.kind.close_delim().is_some() {
            return None;
        }
        if entry.token.kind.open_delim().is_some() && entry.matching as usize >= end {
            return None;
        }
        Some(index)
    }

    pub(super) fn look_ahead_tree_token(&self, n: usize) -> Option<Token> {
        self.tree_index(n).map(|index| self.buffer.tokens[index].token)
    }

    pub(super) fn look_ahead(&self, n: usize) -> Option<TokenTree> {
        self.tree_index(n).map(|index| self.buffer.tree(index))
    }

    pub(super) fn look_ahead_token(&self, dist: usize) -> Token {
        if dist == 0 {
            return Token::dummy();
        }
        self.next_idx
            .checked_add(dist - 1)
            .filter(|&index| index < self.end_idx)
            .and_then(|index| self.buffer.tokens.get(index))
            .map_or_else(|| self.eof(), |entry| entry.token)
    }

    pub(super) fn look_ahead_past_close_delim(&self) -> Option<TokenTree> {
        let open = self.enclosing()?;
        let close = self.buffer.tokens[open].matching as usize;
        let index = close + 1;
        if index >= self.end_idx {
            return None;
        }
        let entry = self.buffer.tokens.get(index)?;
        if entry.token.kind.close_delim().is_some() || entry.token.kind == token::Eof {
            None
        } else {
            Some(self.buffer.tree(index))
        }
    }

    pub(super) fn clone_enclosing_delim(&self) -> TokenTree {
        // After a detached close, enclosing() hides the outer parent. Retain
        // access to this snapshot's own body without exposing that parent.
        let open = self
            .enclosing()
            .or_else(|| {
                (self.end_group != OUTER)
                    .then(|| self.buffer.groups[self.end_group as usize].open_index as usize)
            })
            .expect("cursor must be within a delimited sequence");
        self.buffer.tree(open)
    }

    pub(super) fn bump_to_end(&mut self) {
        self.next_idx = self.end();
    }

    pub(super) fn depth(&self) -> usize {
        self.enclosing()
            .map_or(0, |open| self.buffer.groups[self.buffer.tokens[open].group as usize].depth)
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
        if self.next_idx >= self.end_idx {
            return (self.eof(), Spacing::Alone);
        }
        if let Some(entry) = self.buffer.tokens.get(self.next_idx) {
            self.next_idx += 1;
            (entry.token, entry.spacing)
        } else {
            (self.eof(), Spacing::Alone)
        }
    }
}
