use super::*;
use crate::rustc_span::create_default_session_globals_then;

#[test]
fn without_first_quote_test() {
    create_default_session_globals_then(|| {
        let i = Ident::from_str("'break");
        assert_eq!(i.without_first_quote().name, kw::Break);
    });
}
