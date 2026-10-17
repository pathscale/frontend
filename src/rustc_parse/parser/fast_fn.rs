//! Conservative recognizer for ordinary named function parameters.

use frontend_lex::Kind;

use crate::rustc_ast as ast;
use crate::rustc_ast::token::{IdentIsRaw, Token, TokenKind};
use crate::rustc_parse::parser::oracle::{self, Mode, Site};
use crate::rustc_span::kw;

use super::super::{Parser, fast};
use super::FnParseMode;

impl<'a> Parser<'a> {
    /// Parse the narrow, diagnostic-free form `(ident: SimpleTy, ...)` directly.
    pub(super) fn try_fast_parse_fn_params(
        &mut self,
        fn_parse_mode: &FnParseMode,
    ) -> Option<(ast::ArenaVec<ast::Param>, bool)> {
        let mode = oracle::mode();
        if mode == Mode::Off || oracle::in_shadow_bypass() {
            return None;
        }
        oracle::record_attempt(Site::FnParams);

        let window = fast::RawWindow::from_parser(self)?;
        let accepted = recognize(&window)?;
        if window.token(0)?.0 != self.token {
            return None;
        }
        let shadow = (mode == Mode::Shadow).then(|| self.clone());
        let mut params = ast::ArenaVec::new();
        let mut offset = 1;
        while offset < accepted.close_offset {
            let param_start = offset;
            let is_mut = window.kind(offset) == Some(Kind::Ident)
                && window.token(offset).is_some_and(|(t, _)| t.is_keyword(kw::Mut));
            if is_mut {
                offset += 1;
            }
            let (pat_token, _) = window.token(offset)?;
            let (ident, _) = pat_token.ident()?;
            let pat_start = if is_mut { window.span(param_start)? } else { pat_token.span };
            let pat = ast::ptr::P::new(self.mk_pat(
                pat_start.to(pat_token.span),
                ast::PatKind::Ident(
                    if is_mut { ast::BindingMode::MUT } else { ast::BindingMode::NONE },
                    ident,
                    None,
                ),
            ));
            offset += 2; // pattern identifier and colon
            let (ty_token, _) = window.token(offset)?;
            let (ty_ident, _) = ty_token.ident()?;
            let ty =
                self.mk_ty(ty_token.span, ast::TyKind::Path(None, ast::Path::from_ident(ty_ident)));
            let param_span = pat_start.to(ty_token.span);
            params.push(ast::Param {
                attrs: ast::AttrVec::new(),
                ty,
                pat,
                id: ast::DUMMY_NODE_ID,
                span: param_span,
                is_placeholder: false,
            });
            offset += 1;
            if window.kind(offset) == Some(Kind::Comma) {
                offset += 1;
            }
        }
        let trailing_comma = accepted.close_offset > 1
            && window.kind(accepted.close_offset - 1) == Some(Kind::Comma);
        let has_comma = trailing_comma || params.len() > 1;
        fast::advance_exact(self, accepted.close_offset + 1, None);
        if has_comma {
            // The full sequence parser clears closure recovery state at each comma.
            self.current_closure.take();
        }
        oracle::record_hit(Site::FnParams);

        if let Some(mut reference) = shadow {
            let full = oracle::with_shadow_bypass(|| reference.parse_fn_params_full(fn_parse_mode))
                .expect("accepted simple parameter list must parse in the full parser");
            let same = oracle::same_continuation(Site::FnParams, self, &reference)
                && alloc::format!("{params:#?}") == alloc::format!("{:#?}", full.0)
                && trailing_comma == full.1
                && self.token == reference.token
                && self.prev_token == reference.prev_token
                && self.token_spacing == reference.token_spacing
                && self.num_bump_calls == reference.num_bump_calls
                && self.expected_token_types.iter().eq(reference.expected_token_types.iter());
            if same {
                oracle::record_shadow_ok(Site::FnParams);
            } else {
                oracle::record_shadow_mismatch(Site::FnParams);
                *self = reference;
                return Some(full);
            }
        }
        Some((params, trailing_comma))
    }
}

struct Accepted {
    /// Offset of the closing `)` from the current `(`.
    close_offset: usize,
}

/// Recognize `()` or `( [mut] ident: ident (, [mut] ident: ident)* [,] )`.
fn recognize(window: &fast::RawWindow<'_>) -> Option<Accepted> {
    if window.kind(0)? != Kind::OpenParen {
        return None;
    }
    if window.kind(1)? == Kind::CloseParen {
        return Some(Accepted { close_offset: 1 });
    }

    let mut offset = 1;
    loop {
        if window.kind(offset)? == Kind::Ident
            && window.token(offset).is_some_and(|(t, _)| t.is_keyword(kw::Mut))
        {
            offset += 1;
        }
        if window.kind(offset)? != Kind::Ident
            || !plain_binding(&window.token(offset)?.0)
            || window.kind(offset + 1)? != Kind::Colon
        {
            return None;
        }
        offset += 2;
        if window.kind(offset)? != Kind::Ident
            || !matches!(window.token(offset)?.0.kind, TokenKind::Ident(..))
            || !window.token(offset)?.0.is_path_start()
        {
            return None;
        }
        offset += 1;
        match window.kind(offset)? {
            Kind::CloseParen => return Some(Accepted { close_offset: offset }),
            Kind::Comma => {
                offset += 1;
                if window.kind(offset)? == Kind::CloseParen {
                    return Some(Accepted { close_offset: offset });
                }
            }
            _ => return None,
        }
    }
}

fn plain_binding(token: &Token) -> bool {
    let Some((ident, raw)) = token.ident() else {
        return false;
    };
    matches!(token.kind, TokenKind::Ident(..))
        && !token.is_bool_lit()
        && !token.is_path_segment_keyword()
        && (raw == IdentIsRaw::Yes || !ident.is_reserved())
}
