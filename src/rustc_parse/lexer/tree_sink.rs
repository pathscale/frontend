use alloc::vec::Vec;
use core::ops::Range;

use crate::rustc_ast::token::{Delimiter, Token};
use crate::rustc_ast::tokenstream::{
    DelimSpacing, DelimSpan, FlatTokenBuffer, Spacing, TokenStream, TokenTree,
};

/// Both sinks use the same delimiter diagnostics and token-spacing code.
pub(super) trait TokenTreeSink {
    type Stream;

    fn len(&self) -> usize;
    fn truncate(&mut self, start: usize);
    fn push(&mut self, token: Token, spacing: Spacing);
    fn open(&mut self, token: Token);
    fn finish(&mut self, start: usize) -> Self::Stream;
    fn is_empty(&self, stream: &Self::Stream) -> bool;
    fn first(&self, stream: &Self::Stream) -> Option<Token>;
    fn close(&mut self, span: DelimSpan, spacing: DelimSpacing, delim: Delimiter, stream: Self::Stream);
}

#[derive(Default)]
pub(super) struct TreeSink {
    trees: Vec<TokenTree>,
    empty: Option<TokenStream>,
}

impl TokenTreeSink for TreeSink {
    type Stream = TokenStream;

    fn len(&self) -> usize {
        self.trees.len()
    }

    fn truncate(&mut self, start: usize) {
        self.trees.truncate(start);
    }

    fn push(&mut self, token: Token, spacing: Spacing) {
        self.trees.push(TokenTree::Token(token, spacing));
    }

    fn open(&mut self, _token: Token) {
        // This sink represents the opening delimiter in the tree pushed at close.
    }

    fn finish(&mut self, start: usize) -> TokenStream {
        if start == self.trees.len() {
            return self.empty.get_or_insert_with(TokenStream::default).clone();
        }
        TokenStream::from_exact(self.trees.drain(start..))
    }

    fn is_empty(&self, stream: &TokenStream) -> bool {
        stream.is_empty()
    }

    fn first(&self, stream: &TokenStream) -> Option<Token> {
        match stream.iter().next() {
            Some(TokenTree::Token(token, _)) => Some(*token),
            _ => None,
        }
    }

    fn close(&mut self, span: DelimSpan, spacing: DelimSpacing, delim: Delimiter, stream: TokenStream) {
        self.trees.push(TokenTree::Delimited(span, spacing, delim, stream));
    }
}

impl TokenTreeSink for FlatTokenBuffer {
    type Stream = Range<usize>;

    fn len(&self) -> usize {
        FlatTokenBuffer::len(self)
    }

    fn truncate(&mut self, start: usize) {
        FlatTokenBuffer::truncate(self, start);
    }

    #[inline]
    fn push(&mut self, token: Token, spacing: Spacing) {
        FlatTokenBuffer::push(self, token, spacing);
    }

    fn open(&mut self, token: Token) {
        FlatTokenBuffer::open(self, token);
    }

    fn finish(&mut self, start: usize) -> Self::Stream {
        start..self.len()
    }

    fn is_empty(&self, stream: &Self::Stream) -> bool {
        stream.is_empty()
    }

    fn first(&self, stream: &Self::Stream) -> Option<Token> {
        FlatTokenBuffer::first(self, stream)
    }

    fn close(&mut self, span: DelimSpan, spacing: DelimSpacing, _delim: Delimiter, _stream: Self::Stream) {
        FlatTokenBuffer::close(self, span, spacing);
    }
}
