//! Fast recognition for expression atoms with a non-postfix follower.

use frontend_lex::Kind;

use crate::rustc_ast::ptr::P;
use crate::rustc_ast::token::{IdentIsRaw, TokenKind};
use crate::rustc_ast::{self as ast, Expr, ExprKind};
use crate::rustc_parse::parser::oracle::{self, Mode, Site};

use super::super::{AttrWrapper, Parser, TokenType, fast};

impl<'a> Parser<'a> {
    /// Parse one literal or one ordinary identifier when the next token cannot extend the atom.
    ///
    /// The full parser remains authoritative for every other shape. `Err(attrs)` means that no
    /// parser state was changed and the caller should use its existing path.
    pub(super) fn try_fast_parse_expr_atom(
        &mut self,
        attrs: AttrWrapper,
    ) -> Result<P<Expr>, AttrWrapper> {
        let mode = oracle::mode();
        if mode == Mode::Off || oracle::in_shadow_bypass() || !attrs.is_empty() {
            return Err(attrs);
        }
        oracle::record_attempt(Site::ExprAtom);
        let Some(window) = fast::RawWindow::from_parser(self) else {
            return Err(attrs);
        };
        let Some(kind) = window.kind(0) else {
            return Err(attrs);
        };
        let Some(follower) = window.kind(1) else {
            return Err(attrs);
        };

        let is_literal = kind.is_literal();
        let is_ident = kind == Kind::Ident
            && matches!(self.token.kind, TokenKind::Ident(..))
            && self
                .token
                .ident()
                .is_some_and(|(ident, raw)| raw == IdentIsRaw::Yes || !ident.is_reserved());
        let is_bool = matches!(self.token.kind, TokenKind::Ident(..))
            && self
                .token
                .ident()
                .is_some_and(|(ident, raw)| raw == IdentIsRaw::No && ident.name.is_bool_lit());
        if !(is_literal || is_bool || is_ident) || is_atom_continuation(follower) {
            return Err(attrs);
        }

        if window.token(0).is_none_or(|(token, _)| token != self.token) {
            return Err(attrs);
        }
        let shadow = (mode == Mode::Shadow).then(|| self.clone());
        let lo = self.token.span;
        let expr = if is_literal || is_bool {
            let Some(lit) = self.parse_opt_token_lit().map(|(lit, _)| lit) else {
                return Err(attrs);
            };
            self.mk_expr(lo.to(self.prev_token.span), ExprKind::Lit(lit))
        } else {
            let (ident, _) = self.token.ident().expect("recognized identifier");
            fast::advance_exact(self, 1, None);
            self.mk_expr(lo, ExprKind::Path(None, ast::Path::from_ident(ident)))
        };

        // These are the failed checks performed by parse_expr_path_start and then
        // parse_expr_dot_or_call_with. Successful `bump` calls above clear the incoming set.
        let mask = if is_ident {
            (1u128 << TokenType::PathSep as u32)
                | (1u128 << TokenType::Bang as u32)
                | (1u128 << TokenType::OpenBrace as u32)
                | postfix_expectation_mask()
        } else {
            postfix_expectation_mask()
        };
        self.expected_token_types.insert_mask(mask);

        oracle::record_hit(Site::ExprAtom);
        if let Some(mut reference) = shadow {
            let slow = oracle::with_shadow_bypass(|| {
                reference.parse_expr_dot_or_call(AttrWrapper::empty())
            })
            .expect("accepted expression atom must parse in the full parser");
            let same = oracle::same_continuation(Site::ExprAtom, self, &reference)
                && alloc::format!("{expr:#?}") == alloc::format!("{slow:#?}")
                && self.token == reference.token
                && self.prev_token == reference.prev_token
                && self.token_spacing == reference.token_spacing
                && self.num_bump_calls == reference.num_bump_calls
                && self.expected_token_types.iter().eq(reference.expected_token_types.iter());
            if same {
                oracle::record_shadow_ok(Site::ExprAtom);
            } else {
                oracle::record_shadow_mismatch(Site::ExprAtom);
                *self = reference;
                return Ok(slow);
            }
        }

        Ok(expr)
    }
}

#[inline]
fn postfix_expectation_mask() -> u128 {
    (1u128 << TokenType::Question as u32) | (1u128 << TokenType::Dot as u32)
}

#[inline]
fn is_atom_continuation(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Dot
            | Kind::OpenParen
            | Kind::OpenBracket
            | Kind::Question
            | Kind::OpenBrace
            | Kind::Bang
            | Kind::PathSep
            | Kind::Colon
            | Kind::RArrow
    )
}
