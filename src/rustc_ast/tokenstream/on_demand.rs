use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::token::{self, Delimiter, Token};
use crate::rustc_lexer::FrontmatterAllowed;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::{BytePos, Span};

use frontend_lex::Kind;

use super::lexed_tape::{Group, LexedTape, NO_GROUP, close_delim, delimiter, open_delim};
use super::{DelimSpacing, DelimSpan, Spacing, TokenStream, TokenTree};

#[cfg(any(feature = "parallel", test))]
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
    tape: Arc<LexedTape>,
    idx: u32,
    end_idx: u32,
    enclosing: u32,
    end_group: u32,
    range_eof: Option<(u32, Span)>,
}

impl OnDemandCursor {
    pub(crate) fn new(
        text: Arc<String>,
        start: BytePos,
        begin: u32,
        frontmatter: FrontmatterAllowed,
        edition: Edition,
    ) -> Option<Self> {
        Some(Self::from_tape(LexedTape::build(text, start, begin, frontmatter, edition)?))
    }

    pub(crate) fn from_tape(tape: Arc<LexedTape>) -> Self {
        let end_idx = tape.eof_index();
        Self { tape, idx: 0, end_idx, enclosing: NO_GROUP, end_group: NO_GROUP, range_eof: None }
    }

    pub(crate) fn tape(&self) -> &Arc<LexedTape> {
        &self.tape
    }

    pub(crate) fn raw_identifier_spans(&self) -> &[Span] {
        self.tape.raw_identifier_spans()
    }

    pub(crate) fn depth_zero_cuts(&self) -> &[u32] {
        self.tape.depth_zero_cuts()
    }

    #[inline]
    #[allow(
        dead_code,
        reason = "depth-zero item task cursor API retained for the next parser stage"
    )]
    pub(crate) fn next_index(&self) -> u32 {
        self.idx
    }

    #[inline]
    #[allow(
        dead_code,
        reason = "depth-zero item task cursor API retained for the next parser stage"
    )]
    pub(crate) fn eof_index(&self) -> u32 {
        self.end_idx
    }

    #[inline]
    #[allow(
        dead_code,
        reason = "depth-zero item task cursor API retained for the next parser stage"
    )]
    pub(crate) fn at_file_depth_zero(&self) -> bool {
        self.end_group == NO_GROUP && self.enclosing == NO_GROUP
    }

    #[cold]
    #[inline(never)]
    fn eof(&self) -> (Token, Spacing) {
        let span = self.range_eof.map(|(_, span)| span).unwrap_or_else(|| {
            self.tape.group(self.end_group).map_or_else(
                || self.tape.eof_span(),
                |group| self.tape.token_span(group.close as usize),
            )
        });
        (Token::new(token::Eof, span), Spacing::Alone)
    }

    #[inline]
    fn read(&self, idx: u32) -> (Token, Spacing) {
        if idx >= self.end_idx {
            self.eof()
        } else {
            let index = idx as usize;
            self.tape.materialize_token(index)
        }
    }

    #[inline]
    fn kind(&self) -> Kind {
        if self.idx >= self.end_idx { Kind::Eof } else { self.tape.kind(self.idx as usize) }
    }

    /// Never expose the detached body's parent after returning its close.
    fn active_group(&self) -> Option<&Group> {
        let group = self.tape.group(self.enclosing)?;
        if self.end_group != NO_GROUP {
            let bound = self.tape.group(self.end_group)?;
            if self.idx >= self.end_idx || group.open < bound.open || group.close > bound.close {
                return None;
            }
        }
        Some(group)
    }

    #[inline(always)]
    pub(crate) fn next_and_bump(&mut self) -> (Token, Spacing) {
        if self.idx >= self.end_idx {
            return self.eof();
        }
        let index = self.idx as usize;
        let t = self.tape.token(index);
        let result = self.tape.decode_token(t, index);
        match t.kind {
            Kind::OpenParen | Kind::OpenBrace | Kind::OpenBracket => {
                self.enclosing = t.data;
            }
            Kind::CloseParen | Kind::CloseBrace | Kind::CloseBracket => {
                self.enclosing = self.tape.group(t.data).expect("indexed closing delimiter").parent;
            }
            _ => {}
        }
        self.idx += 1;
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
        if open_delim(self.kind()).is_some() {
            let group_idx = self.tape.token_group(self.idx as usize);
            let group = self.tape.group(group_idx).expect("indexed opening delimiter");
            self.idx = (group.close + 1).min(self.end_idx);
        } else {
            self.next_and_bump();
        }
    }

    fn tree_cursor(&self, n: usize) -> Option<Self> {
        assert_ne!(n, 0);
        let mut cursor = self.clone();
        for _ in 1..n {
            if close_delim(cursor.kind()).is_some() || cursor.kind() == Kind::Eof {
                return None;
            }
            cursor.skip_tree();
        }
        (close_delim(cursor.kind()).is_none() && cursor.kind() != Kind::Eof).then_some(cursor)
    }

    pub(crate) fn look_ahead_tree_token(&self, n: usize) -> Option<Token> {
        let cursor = self.tree_cursor(n)?;
        Some(cursor.read(cursor.idx).0)
    }

    fn take_tree(&mut self) -> TokenTree {
        if let Some(delim) = open_delim(self.kind()) {
            let (open, open_spacing) = self.next_and_bump();
            let mut children = Vec::new();
            while close_delim(self.kind()).is_none() && self.kind() != Kind::Eof {
                children.push(self.take_tree());
            }
            assert_eq!(close_delim(self.kind()), Some(delim));
            let (close, close_spacing) = self.next_and_bump();
            TokenTree::Delimited(
                DelimSpan::from_pair(open.span, close.span),
                DelimSpacing::new(open_spacing, close_spacing),
                delim,
                TokenStream::new(children),
            )
        } else {
            assert!(close_delim(self.kind()).is_none() && self.kind() != Kind::Eof);
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
        cursor.idx = (group.close + 1).min(self.end_idx);
        cursor.enclosing = group.parent;
        if close_delim(cursor.kind()).is_some() || cursor.kind() == Kind::Eof {
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
        cursor.idx = group.open;
        cursor.enclosing = group.parent;
        cursor.take_tree()
    }

    pub(crate) fn bump_to_end(&mut self) {
        self.idx = self.active_group().map_or(self.end_idx, |group| group.close);
    }

    pub(crate) fn depth(&self) -> usize {
        self.active_group().map_or(0, |group| group.depth as usize)
    }

    pub(crate) fn parent_delim_and_span(&self) -> Option<(Delimiter, DelimSpan)> {
        let group = self.active_group()?;
        Some((
            delimiter(group.delim),
            DelimSpan::from_pair(
                self.tape.token_span(group.open as usize),
                self.tape.token_span(group.close as usize),
            ),
        ))
    }

    #[cfg(any(feature = "parallel", test))]
    pub(crate) fn enclosing_group_info(&self) -> Option<EnclosingGroupInfo> {
        let group = self.active_group()?;
        Some(EnclosingGroupInfo {
            group_idx: self.enclosing,
            delim: delimiter(group.delim),
            open: self.tape.token_span(group.open as usize),
            close: self.tape.token_span(group.close as usize),
            inside_bumps: group.close - group.open - 1,
            attr_count: group.attr_count,
        })
    }

    #[cfg(any(feature = "parallel", test))]
    pub(crate) fn detached_for_group(&self, group_idx: u32) -> Option<Self> {
        let group = self.tape.group(group_idx)?;
        let end_idx = group.close.checked_add(1)?;
        if self.idx <= group.open || self.idx > end_idx || end_idx > self.end_idx {
            return None;
        }
        if self.end_group != NO_GROUP {
            let bound = self.tape.group(self.end_group)?;
            if group.open < bound.open || group.close > bound.close {
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
            range_eof: None,
        })
    }

    /// Detach a balanced depth-zero segment `[start, end)`. The cursor's current
    /// token is `start`, so its next-token index must be `start + 1`.
    #[cfg(any(feature = "parallel", test))]
    #[allow(
        dead_code,
        reason = "depth-zero item task cursor API retained for the next parser stage"
    )]
    pub(crate) fn detached_for_range(&self, start: u32, end: u32) -> Option<Self> {
        if self.end_group != NO_GROUP
            || self.enclosing != NO_GROUP
            || start.checked_add(1) != Some(self.idx)
            || start >= end
            || end > self.end_idx
            || !self.is_file_depth_zero_boundary(start)
            || !self.is_file_depth_zero_boundary(end)
        {
            return None;
        }
        let eof_span = self.tape.token_span(end.checked_sub(1)? as usize);
        Some(Self {
            tape: self.tape.clone(),
            idx: self.idx,
            end_idx: end,
            enclosing: NO_GROUP,
            end_group: NO_GROUP,
            range_eof: Some((start, eof_span)),
        })
    }

    #[allow(
        dead_code,
        reason = "depth-zero item task cursor API retained for the next parser stage"
    )]
    fn is_file_depth_zero_boundary(&self, index: u32) -> bool {
        self.tape.groups().iter().all(|group| !(group.open < index && index <= group.close))
    }

    /// Move to the token at `index`, rebuilding the group containing that token.
    #[cfg(any(feature = "parallel", test))]
    #[allow(
        dead_code,
        reason = "depth-zero item task cursor API retained for the next parser stage"
    )]
    pub(crate) fn jump_to(&mut self, index: u32) -> bool {
        if index > self.end_idx || self.range_eof.is_some_and(|(start, _)| index < start) {
            return false;
        }
        if self.end_group != NO_GROUP {
            let Some(bound) = self.tape.group(self.end_group) else { return false };
            if index < bound.open + 1 || index > bound.close + 1 {
                return false;
            }
        }
        self.idx = index;
        self.enclosing = self
            .tape
            .groups()
            .iter()
            .enumerate()
            .filter(|(_, group)| group.open < index && index <= group.close)
            .max_by_key(|(_, group)| group.depth)
            .map_or(NO_GROUP, |(group_idx, _)| group_idx as u32);
        true
    }
}
