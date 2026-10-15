use crate::rustc_ast::token::{self, Delimiter, Token};
use crate::rustc_ast::tokenstream::{TokenStream, TokenTree};
use crate::rustc_data_structures::sync;

fn leaf(tree: Option<&TokenTree>) -> Option<Token> {
    match tree {
        Some(TokenTree::Token(token, _)) => Some(*token),
        _ => None,
    }
}

pub(super) fn count(stream: &TokenStream, count_attrs: bool) -> (u32, u32) {
    let mut bumps = 0u32;
    let mut attributes = 0u32;
    for (index, tree) in stream.iter().enumerate() {
        match tree {
            TokenTree::Token(token, _) => {
                bumps = bumps.checked_add(1).unwrap_or_else(|| sync::retry_parse_serial());
                if count_attrs && matches!(token.kind, token::DocComment(..)) {
                    attributes =
                        attributes.checked_add(1).unwrap_or_else(|| sync::retry_parse_serial());
                }
            }
            TokenTree::Delimited(_, _, delimiter, inner) => {
                let prev = index.checked_sub(1).and_then(|i| leaf(stream.get(i)));
                let prev2 = index.checked_sub(2).and_then(|i| leaf(stream.get(i)));
                let prev3 = index.checked_sub(3).and_then(|i| leaf(stream.get(i)));
                let attribute = *delimiter == Delimiter::Bracket
                    && (prev.is_some_and(|t| t == token::Pound)
                        || prev.is_some_and(|t| t == token::Bang)
                            && prev2.is_some_and(|t| t == token::Pound));
                let invocation = prev.is_some_and(|t| t == token::Bang);
                let macro_rules = prev2.is_some_and(|t| t == token::Bang)
                    && prev3.is_some_and(|t| t.is_ident_named(crate::rustc_span::kw::MacroRules));
                let decl_macro = prev2.is_some_and(|t| t.is_keyword(crate::rustc_span::kw::Macro))
                    || prev3.is_some_and(|t| t.is_keyword(crate::rustc_span::kw::Macro));
                let (nested_bumps, nested_attrs) = count(
                    inner,
                    count_attrs && !attribute && !invocation && !macro_rules && !decl_macro,
                );
                bumps = bumps
                    .checked_add(nested_bumps)
                    .and_then(|n| n.checked_add(if delimiter.skip() { 0 } else { 2 }))
                    .unwrap_or_else(|| sync::retry_parse_serial());
                attributes = attributes
                    .checked_add(nested_attrs)
                    .and_then(|n| n.checked_add(u32::from(count_attrs && attribute)))
                    .unwrap_or_else(|| sync::retry_parse_serial());
            }
        }
    }
    (bumps, attributes)
}

// Speculation is only for source tokens. Expanded tokens can mutate hygiene and metavariable
// state, which deliberately remains on the established parser path.
pub(super) fn source_tokens(stream: &TokenStream) -> bool {
    let valid = |span: crate::rustc_span::Span| {
        !span.is_dummy() && span.ctxt().is_root() && span.parent().is_none()
    };
    stream.iter().all(|tree| match tree {
        TokenTree::Token(value, _) => {
            valid(value.span)
                && !matches!(
                    value.kind,
                    token::NtIdent(..)
                        | token::NtLifetime(..)
                        | token::OpenInvisible(..)
                        | token::CloseInvisible(..)
                )
        }
        TokenTree::Delimited(spans, _, delimiter, inner) => {
            !matches!(delimiter, Delimiter::Invisible(..))
                && valid(spans.open)
                && valid(spans.close)
                && source_tokens(inner)
        }
    })
}

pub(super) fn cursor_can_parallel_parse(
    cursor: &crate::rustc_ast::tokenstream::TokenCursor,
) -> bool {
    cursor.can_parallel_parse() || cursor.source_stream().is_some_and(source_tokens)
}
