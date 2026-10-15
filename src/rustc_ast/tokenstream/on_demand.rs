use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::token::{self, Delimiter, Token};
use crate::rustc_lexer::FrontmatterAllowed;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::{BytePos, Span};

use super::indexed_tape::{Group, IndexedSourceTape, NO_GROUP};
use super::source_token::SourceKind;
use super::{DelimSpacing, DelimSpan, Spacing, TokenStream, TokenTree};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EnclosingGroupInfo {
    pub(crate) group_idx: u32,
    pub(crate) delim: Delimiter,
    pub(crate) open: Span,
    pub(crate) close: Span,
    pub(crate) inside_bumps: u32,
    pub(crate) attr_count: u32,
}

/// Snapshots share an immutable tape. `idx` is Parser's next token, not its
/// current token. `end_group` retains the detached EOF span after its close.
#[derive(Clone, Debug)]
pub(crate) struct OnDemandCursor {
    tape: Arc<IndexedSourceTape>,
    idx: u32,
    end_idx: u32,
    enclosing: u32,
    end_group: u32,
}

impl OnDemandCursor {
    pub(crate) fn new(
        text: Arc<String>,
        start: BytePos,
        begin: u32,
        frontmatter: FrontmatterAllowed,
        edition: Edition,
    ) -> Option<Self> {
        Some(Self::from_tape(IndexedSourceTape::build(text, start, begin, frontmatter, edition)?))
    }

    pub(crate) fn from_tape(tape: Arc<IndexedSourceTape>) -> Self {
        let end_idx = u32::try_from(tape.token_count().checked_sub(1).expect("tape has EOF"))
            .expect("tape index fits u32");
        Self { tape, idx: 0, end_idx, enclosing: NO_GROUP, end_group: NO_GROUP }
    }

    pub(crate) fn tape(&self) -> &Arc<IndexedSourceTape> {
        &self.tape
    }

    pub(crate) fn raw_identifier_spans(&self) -> &[Span] {
        &self.tape.raw_identifier_spans
    }

    pub(crate) fn depth_zero_cuts(&self) -> &[u32] {
        self.tape.depth_zero_cuts()
    }

    fn eof(&self) -> (Token, Spacing) {
        let span = self
            .tape
            .group(self.end_group)
            .map_or_else(|| self.tape.eof_span(), |group| group.close);
        (Token::new(token::Eof, span), Spacing::Alone)
    }

    fn read(&self, idx: u32) -> (Token, Spacing) {
        if idx >= self.end_idx { self.eof() } else { self.tape.materialize_token(idx as usize) }
    }

    fn kind(&self) -> SourceKind {
        if self.idx >= self.end_idx {
            SourceKind::Eof
        } else {
            self.tape.tokens[self.idx as usize].kind()
        }
    }

    /// Never expose the detached body's parent after returning its close.
    fn active_group(&self) -> Option<&Group> {
        let group = self.tape.group(self.enclosing)?;
        if self.end_group != NO_GROUP {
            let bound = self.tape.group(self.end_group)?;
            if self.idx >= self.end_idx
                || group.open_idx < bound.open_idx
                || group.close_idx > bound.close_idx
            {
                return None;
            }
        }
        Some(group)
    }

    #[inline]
    pub(crate) fn next_and_bump(&mut self) -> (Token, Spacing) {
        let result = self.read(self.idx);
        if self.idx < self.end_idx {
            let kind = self.tape.tokens[self.idx as usize].kind();
            if kind.open_delim().is_some() {
                self.enclosing = self.tape.token_group(self.idx as usize);
            } else if kind.close_delim().is_some() {
                let group_idx = self.tape.token_group(self.idx as usize);
                self.enclosing =
                    self.tape.group(group_idx).expect("indexed closing delimiter").parent;
            }
            self.idx += 1;
        }
        result
    }

    pub(crate) fn look_ahead_token(&self, dist: usize) -> Token {
        if dist == 0 {
            return Token::dummy();
        }
        let idx = u32::try_from(dist - 1)
            .ok()
            .and_then(|offset| self.idx.checked_add(offset))
            .unwrap_or(self.end_idx)
            .min(self.end_idx);
        self.read(idx).0
    }

    fn skip_tree(&mut self) {
        if self.kind().open_delim().is_some() {
            let group_idx = self.tape.token_group(self.idx as usize);
            let group = self.tape.group(group_idx).expect("indexed opening delimiter");
            self.idx = (group.close_idx + 1).min(self.end_idx);
        } else {
            self.next_and_bump();
        }
    }

    fn tree_cursor(&self, n: usize) -> Option<Self> {
        assert_ne!(n, 0);
        let mut cursor = self.clone();
        for _ in 1..n {
            if cursor.kind().close_delim().is_some() || cursor.kind() == SourceKind::Eof {
                return None;
            }
            cursor.skip_tree();
        }
        (cursor.kind().close_delim().is_none() && cursor.kind() != SourceKind::Eof)
            .then_some(cursor)
    }

    pub(crate) fn look_ahead_tree_token(&self, n: usize) -> Option<Token> {
        let cursor = self.tree_cursor(n)?;
        Some(cursor.read(cursor.idx).0)
    }

    fn take_tree(&mut self) -> TokenTree {
        if let Some(delim) = self.kind().open_delim() {
            let (open, open_spacing) = self.next_and_bump();
            let mut children = Vec::new();
            while self.kind().close_delim().is_none() && self.kind() != SourceKind::Eof {
                children.push(self.take_tree());
            }
            assert_eq!(self.kind().close_delim(), Some(delim));
            let (close, close_spacing) = self.next_and_bump();
            TokenTree::Delimited(
                DelimSpan::from_pair(open.span, close.span),
                DelimSpacing::new(open_spacing, close_spacing),
                delim,
                TokenStream::new(children),
            )
        } else {
            assert!(self.kind().close_delim().is_none() && self.kind() != SourceKind::Eof);
            let (token, spacing) = self.next_and_bump();
            TokenTree::Token(token, spacing)
        }
    }

    pub(crate) fn look_ahead(&self, n: usize) -> Option<TokenTree> {
        Some(self.tree_cursor(n)?.take_tree())
    }

    pub(crate) fn look_ahead_past_close_delim(&self) -> Option<TokenTree> {
        let group = *self.active_group()?;
        let mut cursor = self.clone();
        cursor.idx = (group.close_idx + 1).min(self.end_idx);
        cursor.enclosing = group.parent;
        if cursor.kind().close_delim().is_some() || cursor.kind() == SourceKind::Eof {
            None
        } else {
            Some(cursor.take_tree())
        }
    }

    pub(crate) fn clone_enclosing_delim(&self) -> TokenTree {
        // An exhausted detached snapshot may reconstruct its own body, but
        // cannot reconstruct the outer parent now stored in `enclosing`.
        let group = *self
            .active_group()
            .or_else(|| self.tape.group(self.end_group))
            .expect("cursor has an enclosing delimiter");
        let mut cursor = self.clone();
        cursor.idx = group.open_idx;
        cursor.enclosing = group.parent;
        cursor.take_tree()
    }

    pub(crate) fn bump_to_end(&mut self) {
        self.idx = self.active_group().map_or(self.end_idx, |group| group.close_idx);
    }

    pub(crate) fn depth(&self) -> usize {
        self.active_group().map_or(0, |group| group.depth as usize)
    }

    pub(crate) fn parent_delim_and_span(&self) -> Option<(Delimiter, DelimSpan)> {
        let group = self.active_group()?;
        Some((group.delim, DelimSpan::from_pair(group.open, group.close)))
    }

    pub(crate) fn enclosing_group_info(&self) -> Option<EnclosingGroupInfo> {
        let group = self.active_group()?;
        Some(EnclosingGroupInfo {
            group_idx: self.enclosing,
            delim: group.delim,
            open: group.open,
            close: group.close,
            inside_bumps: group.inside_bumps,
            attr_count: group.attr_count,
        })
    }

    pub(crate) fn detached_for_group(&self, group_idx: u32) -> Option<Self> {
        let group = self.tape.group(group_idx)?;
        let end_idx = group.close_idx.checked_add(1)?;
        if self.idx <= group.open_idx || self.idx > end_idx || end_idx > self.end_idx {
            return None;
        }
        if self.end_group != NO_GROUP {
            let bound = self.tape.group(self.end_group)?;
            if group.open_idx < bound.open_idx || group.close_idx > bound.close_idx {
                return None;
            }
        }
        // Balanced tape intervals prove membership while inside the group.
        // At one-past-close, Parser must already own this group's close.
        if self.idx == end_idx && self.enclosing != group.parent {
            return None;
        }
        Some(Self {
            tape: self.tape.clone(),
            idx: self.idx,
            end_idx,
            enclosing: self.enclosing,
            end_group: group_idx,
        })
    }
}
