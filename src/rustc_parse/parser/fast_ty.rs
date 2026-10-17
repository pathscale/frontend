//! Conservative raw-token fast paths for common type shapes.

use alloc::format;

use frontend_lex::Kind;

use crate::rustc_ast::{
    self as ast, MutTy, Mutability, Path, PathSegment, PathSegments, Ty, TyKind,
};
use crate::rustc_parse::parser::oracle::{self, Mode, Site};
use crate::rustc_span::{Span, kw};

use super::super::Parser;
use super::super::fast::{self, RawWindow};
use super::{AllowPlus, RecoverQPath, RecoverQuestionMark, RecoverReturnSign};

const PATH_EXPECTATIONS: u128 = (1u128 << super::super::TokenType::Lt as u32)
    | (1u128 << super::super::TokenType::OpenParen as u32)
    | (1u128 << super::super::TokenType::PathSep as u32)
    | (1u128 << super::super::TokenType::Bang as u32)
    | (1u128 << super::super::TokenType::Plus as u32);

const BORROWED_PATH_EXPECTATIONS: u128 =
    PATH_EXPECTATIONS & !(1u128 << super::super::TokenType::Plus as u32);

#[derive(Clone, Copy)]
enum SimpleShape {
    Infer,
    Never,
    Unit,
    Path { start: usize, end: usize },
    Borrowed { inner: SimpleInner, inner_start: Span, inner_end: Span, mutability: Mutability },
}

#[derive(Clone, Copy)]
enum SimpleInner {
    Infer,
    Never,
    Unit,
    Path { start: usize, end: usize },
}

/// Parse only clean, attribute-free source types whose full grammar has no ambiguity.
pub(super) fn try_fast_parse_ty(
    parser: &mut Parser<'_>,
    allow_plus: AllowPlus,
    recover_qpath: RecoverQPath,
    recover_return_sign: RecoverReturnSign,
) -> Option<crate::rustc_ast::ptr::P<Ty>> {
    let mode = oracle::mode();
    if mode == Mode::Off || oracle::in_shadow_bypass() {
        return None;
    }
    oracle::record_attempt(Site::Ty);

    // Shadow from the entry state so the normal parser owns an independent cursor and masks.
    let mut shadow = (mode == Mode::Shadow).then(|| parser.clone());
    let window = fast::RawWindow::from_parser(parser)?;
    let (shape, offset, mask, start_span, end_span) = if window.kind(0) == Some(Kind::And) {
        let start_span = window.span(0)?;
        let mut inner_offset = 1;
        let mut mutability = Mutability::Not;
        if window.kind(inner_offset) == Some(Kind::Ident)
            && window.token(inner_offset).is_some_and(|(token, _)| token.is_keyword(kw::Mut))
        {
            mutability = Mutability::Mut;
            inner_offset += 1;
        }
        let inner = recognize_simple(&window, inner_offset)?;
        if !is_clean_terminator(&window, inner.end)? {
            return None;
        }
        let inner_start = window.span(inner_offset)?;
        let inner_end = window.span(inner.end - 1)?;
        let mask = match inner.shape {
            SimpleShape::Path { .. } => BORROWED_PATH_EXPECTATIONS,
            _ => 0,
        };
        (
            SimpleShape::Borrowed {
                inner: inner.shape.into_inner()?,
                inner_start,
                inner_end,
                mutability,
            },
            inner.end,
            mask,
            start_span,
            inner_end,
        )
    } else {
        let simple = recognize_simple(&window, 0)?;
        if !is_clean_terminator(&window, simple.end)? {
            return None;
        }
        let end_span = window.span(simple.end - 1)?;
        let mask = match simple.shape {
            SimpleShape::Path { .. } if allow_plus == AllowPlus::Yes => PATH_EXPECTATIONS,
            SimpleShape::Path { .. } => BORROWED_PATH_EXPECTATIONS,
            _ => 0,
        };
        (simple.shape, simple.end, mask, window.span(0)?, end_span)
    };

    if window.token(0)?.0 != parser.token {
        return None;
    }
    let kind = materialize_shape(&window, parser, shape)?;
    fast::advance_exact(parser, offset, Some(mask));
    let ty = parser.mk_ty(start_span.to(end_span), kind);
    oracle::record_hit(Site::Ty);

    if let Some(mut shadow_parser) = shadow.take() {
        let slow_ty = oracle::with_shadow_bypass(|| {
            shadow_parser
                .parse_ty_common(
                    allow_plus,
                    super::AllowCVariadic::No,
                    recover_qpath,
                    recover_return_sign,
                    None,
                    RecoverQuestionMark::Yes,
                )
                .expect("accepted clean type parses in the full parser")
        });
        let same = oracle::same_continuation(Site::Ty, parser, &shadow_parser)
            && format!("{ty:?}") == format!("{slow_ty:?}");
        if same {
            oracle::record_shadow_ok(Site::Ty);
        } else {
            oracle::record_shadow_mismatch(Site::Ty);
            *parser = shadow_parser;
            return Some(slow_ty);
        }
    }
    Some(ty)
}

// SimpleShape carries the source end for paths. This wrapper keeps the recognition result small.
struct Recognized {
    shape: SimpleShape,
    end: usize,
}

impl SimpleShape {
    fn path_end(self) -> Option<usize> {
        match self {
            Self::Path { end, .. } => Some(end),
            _ => None,
        }
    }

    fn into_inner(self) -> Option<SimpleInner> {
        match self {
            Self::Infer => Some(SimpleInner::Infer),
            Self::Never => Some(SimpleInner::Never),
            Self::Unit => Some(SimpleInner::Unit),
            Self::Path { start, end } => Some(SimpleInner::Path { start, end }),
            Self::Borrowed { .. } => None,
        }
    }
}

fn recognize_simple(window: &RawWindow<'_>, start: usize) -> Option<Recognized> {
    let shape = match window.kind(start)? {
        Kind::Bang => SimpleShape::Never,
        Kind::OpenParen if window.kind(start + 1) == Some(Kind::CloseParen) => SimpleShape::Unit,
        Kind::Ident => {
            let (first, _) = window.token(start)?;
            if first.is_keyword(kw::Underscore) {
                SimpleShape::Infer
            } else {
                let end = ident_path_end(window, start)?;
                // Validate every segment using rustc's edition and keyword rules.
                let mut segment = start;
                while segment < end {
                    let (token, _) = window.token(segment)?;
                    if !token.is_path_start() {
                        return None;
                    }
                    segment += 1;
                    if segment < end {
                        if window.kind(segment) != Some(Kind::PathSep) {
                            return None;
                        }
                        segment += 1;
                    }
                }
                SimpleShape::Path { start, end }
            }
        }
        _ => return None,
    };
    let end = shape.path_end().unwrap_or_else(|| match shape {
        SimpleShape::Unit => start + 2,
        _ => start + 1,
    });
    Some(Recognized { shape, end })
}

fn is_clean_terminator(window: &RawWindow<'_>, offset: usize) -> Option<bool> {
    let kind = window.kind(offset)?;
    Some(
        matches!(
            kind,
            Kind::Comma
                | Kind::CloseParen
                | Kind::Gt
                | Kind::Eq
                | Kind::Semi
                | Kind::OpenBrace
                | Kind::RArrow
                | Kind::CloseBracket
                | Kind::Colon
        ) || (kind == Kind::Ident
            && window.token(offset).is_some_and(|(token, _)| token.is_keyword(kw::Where))),
    )
}

fn materialize_shape(
    window: &RawWindow<'_>,
    parser: &Parser<'_>,
    shape: SimpleShape,
) -> Option<TyKind> {
    Some(match shape {
        SimpleShape::Infer => TyKind::Infer,
        SimpleShape::Never => TyKind::Never,
        SimpleShape::Unit => TyKind::Tup(ast::ArenaVec::new()),
        SimpleShape::Path { start, end } => path_kind(window, start, end)?,
        SimpleShape::Borrowed { inner, inner_start, inner_end, mutability } => {
            let inner_kind = match inner {
                SimpleInner::Infer => TyKind::Infer,
                SimpleInner::Never => TyKind::Never,
                SimpleInner::Unit => TyKind::Tup(ast::ArenaVec::new()),
                SimpleInner::Path { start, end } => path_kind(window, start, end)?,
            };
            let inner_ty = parser.mk_ty(inner_start.to(inner_end), inner_kind);
            TyKind::Ref(None, MutTy { ty: inner_ty, mutbl: mutability })
        }
    })
}

fn path_kind(window: &RawWindow<'_>, start: usize, end: usize) -> Option<TyKind> {
    let mut segments = PathSegments::new();
    let mut offset = start;
    while offset < end {
        let (token, _) = window.token(offset)?;
        let (ident, _) = token.ident()?;
        segments.push(PathSegment::from_ident(ident));
        offset += 1;
        if offset < end {
            offset += 1; // The accepted shape guarantees a `::` here.
        }
    }
    let span = window.span(start)?.to(window.span(end - 1)?);
    Some(TyKind::Path(None, Path { segments, span }))
}

fn ident_path_end(window: &RawWindow<'_>, start: usize) -> Option<usize> {
    if !window.is_ident(start) {
        return None;
    }
    let mut offset = start + 1;
    loop {
        match window.kind(offset) {
            Some(Kind::PathSep) if window.is_ident(offset + 1) => offset += 2,
            Some(Kind::PathSep | Kind::Lt | Kind::Bang) => return None,
            _ => return Some(offset),
        }
    }
}
