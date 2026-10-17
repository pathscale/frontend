//! Direct parsing for simple source-backed `let` declarations.

use crate::rustc_ast::ptr::P;
use crate::rustc_ast::token::{self, IdentIsRaw, Token, TokenKind};
use crate::rustc_ast::{
    self as ast, AttrVec, BindingMode, DUMMY_NODE_ID, Local, LocalKind, PatKind, TyKind,
};
use crate::rustc_errors::PResult;
use crate::rustc_parse::parser::oracle::{self, Mode, Site};
use crate::rustc_span::kw;
use frontend_lex::Kind;

use super::super::Parser;
use super::super::fast::{self, RawWindow};
use crate::exp;

impl<'a> Parser<'a> {
    /// Parse `let [mut] ident [: SimpleTy] = Expr;` without the pattern/type parsers.
    pub(super) fn try_fast_parse_local(&mut self, attrs: AttrVec) -> Result<P<Local>, AttrVec> {
        let mode = oracle::mode();
        if mode == Mode::Off
            || oracle::in_shadow_bypass()
            || !attrs.is_empty()
            || !fast::is_plain_source_token(self)
        {
            return Err(attrs);
        }
        oracle::record_attempt(Site::LetStmt);
        let Some(window) = RawWindow::from_parser(self) else {
            return Err(attrs);
        };
        let Some(accepted) = recognize(&window, self.token.is_keyword(kw::Mut)) else {
            return Err(attrs);
        };

        if window.token(0).is_none_or(|(token, _)| token != self.token) {
            return Err(attrs);
        }
        let original = (mode == Mode::Shadow).then(|| self.clone());
        let snapshot = self.clone();
        let prefix = self.fast_local_prefix(&window, &accepted);
        let fast_result = self.parse_fast_local_candidate(attrs.clone(), prefix, accepted);
        let local = match fast_result {
            Ok(Some(local)) => local,
            Ok(None) => {
                *self = snapshot;
                return Err(attrs);
            }
            Err(err) => {
                err.cancel();
                *self = snapshot;
                return Err(attrs);
            }
        };
        oracle::record_hit(Site::LetStmt);

        if let Some(mut reference) = original {
            let full =
                oracle::with_shadow_bypass(|| reference.parse_local_full(None, attrs.clone()));
            let same = full.as_ref().is_ok_and(|full_local| {
                oracle::same_continuation(Site::LetStmt, self, &reference)
                    && alloc::format!("{local:#?}") == alloc::format!("{full_local:#?}")
                    && self.token == reference.token
                    && self.prev_token == reference.prev_token
                    && self.token_spacing == reference.token_spacing
                    && self.num_bump_calls == reference.num_bump_calls
                    && self.expected_token_types.iter().eq(reference.expected_token_types.iter())
            });
            if same {
                oracle::record_shadow_ok(Site::LetStmt);
            } else {
                oracle::record_shadow_mismatch(Site::LetStmt);
                match full {
                    Ok(full_local) => {
                        *self = reference;
                        return Ok(full_local);
                    }
                    Err(err) => {
                        err.cancel();
                        *self = snapshot;
                        return Err(attrs);
                    }
                }
            }
        }
        Ok(local)
    }

    fn fast_local_prefix(
        &self,
        window: &RawWindow<'_>,
        accepted: &Accepted,
    ) -> (P<ast::Pat>, Option<P<ast::Ty>>, Option<crate::rustc_span::Span>) {
        let mut pat_offset = 0;
        let is_mut = self.token.is_keyword(kw::Mut);
        let pat_start = window.span(0).expect("recognized pattern span");
        if is_mut {
            pat_offset = 1;
        }
        let (pat_token, _) = window.token(pat_offset).expect("recognized pattern token");
        let (ident, _) = pat_token.ident().expect("recognized binding identifier");
        let pat = P::new(self.mk_pat(
            pat_start.to(pat_token.span),
            PatKind::Ident(if is_mut { BindingMode::MUT } else { BindingMode::NONE }, ident, None),
        ));
        let mut offset = pat_offset + 1;
        let (ty, colon_sp) = if accepted.has_type {
            let colon = window.span(offset).expect("recognized colon");
            offset += 1;
            let (ty_token, _) = window.token(offset).expect("recognized simple type");
            let (ty_ident, _) = ty_token.ident().expect("recognized type identifier");
            (
                Some(
                    self.mk_ty(ty_token.span, TyKind::Path(None, ast::Path::from_ident(ty_ident))),
                ),
                Some(colon),
            )
        } else {
            (None, None)
        };

        (pat, ty, colon_sp)
    }

    fn parse_fast_local_candidate(
        &mut self,
        attrs: AttrVec,
        (pat, ty, colon_sp): (P<ast::Pat>, Option<P<ast::Ty>>, Option<crate::rustc_span::Span>),
        accepted: Accepted,
    ) -> PResult<'a, Option<P<Local>>> {
        let lo = self.prev_token.span;
        // Consume the binding, optional type, and `=` with the same bump sequence as the full
        // parser. The expression remains owned by the normal expression parser.
        fast::advance_exact(self, accepted.eq_offset, None);
        let Some(init) = self.parse_initializer(false)? else {
            return Ok(None);
        };
        if self.eat_keyword(exp!(Else)) || self.token != token::Semi {
            return Ok(None);
        }
        let hi = self.token.span;
        Ok(Some(P::new(ast::Local {
            super_: None,
            ty,
            pat,
            kind: LocalKind::Init(init),
            id: DUMMY_NODE_ID,
            span: lo.to(hi),
            colon_sp,
            attrs,
            tokens: None,
        })))
    }
}

struct Accepted {
    has_type: bool,
    eq_offset: usize,
}

// Restrict the initializer before changing state. Arbitrary expressions may publish
// diagnostics, attributes or feature gates and cannot be rolled back with a Parser clone.
fn recognize(window: &RawWindow<'_>, starts_mut: bool) -> Option<Accepted> {
    let accepted = recognize_prefix(window, starts_mut)?;
    let init = window.token(accepted.eq_offset + 1)?.0;
    if !(window.kind(accepted.eq_offset + 1)?.is_literal()
        || init.is_bool_lit()
        || plain_binding(&init))
        || window.kind(accepted.eq_offset + 2)? != Kind::Semi
    {
        return None;
    }
    Some(accepted)
}

fn recognize_prefix(window: &RawWindow<'_>, starts_mut: bool) -> Option<Accepted> {
    let pat_offset = usize::from(starts_mut);
    if !window.is_ident(pat_offset)
        || !window.token(pat_offset).is_some_and(|(token, _)| plain_binding(&token))
    {
        return None;
    }
    let after_pat = pat_offset + 1;
    match window.kind(after_pat)? {
        Kind::Eq => Some(Accepted { has_type: false, eq_offset: after_pat }),
        Kind::Colon => {
            let ty_offset = after_pat + 1;
            if !window.is_ident(ty_offset)
                || !window.token(ty_offset).is_some_and(|(token, _)| token.is_path_start())
            {
                return None;
            }
            let eq_offset = ty_offset + 1;
            (window.kind(eq_offset) == Some(Kind::Eq))
                .then_some(Accepted { has_type: true, eq_offset })
        }
        _ => None,
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
