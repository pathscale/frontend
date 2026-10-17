//! Source-backed fast parsing for simple binding patterns.

use frontend_lex::Kind;

use crate::rustc_ast::ptr::P;
use crate::rustc_ast::token::{IdentIsRaw, Token, TokenKind};
use crate::rustc_ast::{self as ast, BindingMode, Pat, PatKind};
use crate::rustc_parse::parser::oracle::{self, Mode, Site};
use crate::rustc_parse::parser::token_type::TokenType;
use crate::rustc_span::{Ident, Span, kw};

use super::super::fast::{self, RawWindow};
use super::{Expected, Parser, PatternLocation};

struct Accepted {
    pat_kind: PatKind,
    span: Span,
    token_count: usize,
    final_mask: u128,
}

fn simple_binding_ident(token: &Token) -> Option<Ident> {
    let (ident, is_raw) = token.ident()?;
    if token.is_bool_lit()
        || token.is_path_segment_keyword()
        || token.is_keyword(kw::Ref)
        || token.is_keyword(kw::Box)
        || token.is_keyword(kw::Const)
        || token.is_keyword(kw::Mut)
        || token.is_keyword(kw::In)
        || (is_raw == IdentIsRaw::No && ident.is_reserved())
        || matches!(
            token.kind,
            TokenKind::NtIdent(..)
                | TokenKind::NtLifetime(..)
                | TokenKind::OpenInvisible(..)
                | TokenKind::CloseInvisible(..)
        )
    {
        return None;
    }
    Some(ident)
}

fn accepted_follower(token: &Token) -> bool {
    matches!(
        token.kind,
        TokenKind::Colon
            | TokenKind::Eq
            | TokenKind::Comma
            | TokenKind::CloseParen
            | TokenKind::Semi
    ) || token.is_keyword(kw::In)
}

fn recognize(window: &RawWindow<'_>) -> Option<Accepted> {
    let first = window.token(0)?.0;
    let (pat_kind, span, token_count, final_mask) = if first.is_keyword(kw::Underscore) {
        let follower = window.token(1)?.0;
        if !accepted_follower(&follower) {
            return None;
        }
        (PatKind::Wild, first.span, 1, 0)
    } else if first.is_keyword(kw::Mut) {
        if window.kind(1)? != Kind::Ident {
            return None;
        }
        let binding = simple_binding_ident(&window.token(1)?.0)?;
        let follower = window.token(2)?.0;
        if !accepted_follower(&follower) {
            return None;
        }
        (
            PatKind::Ident(BindingMode::MUT, binding, None),
            first.span.to(binding.span),
            2,
            1u128 << TokenType::At as u32,
        )
    } else {
        if window.kind(0)? != Kind::Ident {
            return None;
        }
        let binding = simple_binding_ident(&first)?;
        let follower = window.token(1)?.0;
        if !accepted_follower(&follower) {
            return None;
        }
        (
            PatKind::Ident(BindingMode::NONE, binding, None),
            binding.span,
            1,
            1u128 << TokenType::At as u32,
        )
    };
    Some(Accepted { pat_kind, span, token_count, final_mask })
}

/// Parse `_`, `ident`, or `mut ident` on a clean Source cursor when its follower is unambiguous.
impl<'a> Parser<'a> {
    pub(super) fn try_fast_parse_pat(
        &mut self,
        allow_range_pat: bool,
        expected: Option<Expected>,
        syntax_loc: Option<PatternLocation>,
    ) -> Option<P<ast::Pat>> {
        let mode = oracle::mode();
        if mode == Mode::Off || oracle::in_shadow_bypass() {
            return None;
        }
        oracle::record_attempt(Site::Pat);

        let window = fast::RawWindow::from_parser(self)?;
        let accepted = recognize(&window)?;
        if window.token(0)?.0 != self.token {
            return None;
        }
        let mode = oracle::mode();
        let shadow_before = if mode == Mode::Shadow { Some(self.clone()) } else { None };

        fast::advance_exact(self, accepted.token_count, Some(accepted.final_mask));
        let fast_pat = P::new(self.mk_pat(accepted.span, accepted.pat_kind));
        oracle::record_hit(Site::Pat);

        if let Some(mut shadow) = shadow_before {
            let shadow_pat = oracle::with_shadow_bypass(|| {
                shadow
                    .parse_pat_with_range_pat(allow_range_pat, expected, syntax_loc)
                    .expect("accepted pattern window must parse without diagnostics")
            });
            let same_ast = oracle::same_continuation(Site::Pat, self, &shadow)
                && format_pat(&fast_pat) == alloc::format!("{shadow_pat:#?}");
            if !same_ast {
                oracle::record_shadow_mismatch(Site::Pat);
                *self = shadow;
                return Some(P::new(shadow_pat));
            }
            if oracle::same_continuation(Site::Pat, self, &shadow) {
                oracle::record_shadow_ok(Site::Pat);
            } else {
                oracle::record_shadow_mismatch(Site::Pat);
                *self = shadow;
                return Some(P::new(shadow_pat));
            }
        }

        Some(fast_pat)
    }
}

fn format_pat(pat: &P<Pat>) -> alloc::string::String {
    alloc::format!("{pat:#?}")
}
