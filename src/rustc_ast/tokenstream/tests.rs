use crate::rustc_span::DUMMY_SP;

use crate::rustc_ast::token::TokenKind;
use crate::rustc_ast::tokenstream::TokenStream;

#[test]
fn test_token_stream_iter() {
    let ts = TokenStream::token_alone(TokenKind::Eq, DUMMY_SP);
    assert_eq!(ts.len(), 1);

    let iter = ts.iter();
    assert_eq!(iter.size_hint(), (1, Some(1)));
}
