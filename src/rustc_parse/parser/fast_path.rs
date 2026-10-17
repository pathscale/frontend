use frontend_lex::Kind;

use crate::rustc_ast::token::{IdentIsRaw, Token, TokenKind};
use crate::rustc_ast::{Path, PathSegment, PathSegments};
use crate::rustc_parse::parser::oracle::{self, Mode, Site};
use crate::rustc_span::Ident;

use super::super::{Parser, fast};
use super::PathStyle;

impl<'a> Parser<'a> {
    /// Parse a path made only from plain source identifier segments.
    ///
    /// Recognition does not change parser state. Once the whole path and its follower have
    /// passed the grammar-specific checks, tokens are consumed through the ordinary bump path.
    pub(super) fn try_fast_parse_path(&mut self, style: PathStyle) -> Option<Path> {
        if oracle::in_shadow_bypass() || oracle::mode() == Mode::Off {
            return None;
        }
        oracle::record_attempt(Site::Path);

        let window = fast::RawWindow::from_parser(self)?;
        let token_count = window.simple_ident_path()?;
        let follower = window.kind(token_count)?;
        let (first_token, _) = window.token(0)?;
        if first_token != self.token {
            return None;
        }

        // Generic arguments, a macro invocation, or a further separator need the full grammar.
        if matches!(follower, Kind::PathSep | Kind::Lt | Kind::Bang) {
            return None;
        }
        if follower == Kind::OpenParen
            && matches!(style, PathStyle::Type | PathStyle::Pat | PathStyle::Expr)
        {
            return None;
        }

        // Expression paths recover a single colon followed by an identifier. Keep that case on
        // the full parser because it consumes the colon and may emit a diagnostic.
        if style == PathStyle::Expr && follower == Kind::Colon {
            let Some((next, _)) = window.token(token_count + 1) else {
                return None;
            };
            if next.is_non_reserved_ident() {
                return None;
            }
        }

        let shadow_parser = (oracle::mode() == Mode::Shadow).then(|| self.clone());
        let mut segments = PathSegments::new();
        let mut first_span = None;
        let mut last_span = None;
        let mut offset = 0;
        while offset < token_count {
            let (token, _) = window.token(offset)?;
            let (ident, raw) = token.ident()?;
            if !is_plain_path_ident(&token, ident, raw) {
                return None;
            }
            first_span.get_or_insert(token.span);
            last_span = Some(token.span);
            segments.push(PathSegment::from_ident(ident));
            offset += 2;
        }

        let span = first_span?.to(last_span?);
        let path = Path { segments, span };
        fast::advance_exact(self, token_count, Some(final_expectation_mask(style)));
        oracle::record_hit(Site::Path);

        if let Some(mut full_parser) = shadow_parser {
            // The accepted shape contains only clean, attribute-free identifiers. Replaying it
            // cannot emit diagnostics or allocate AttrIds, so the original session is safe.
            let full_path =
                oracle::with_shadow_bypass(|| full_parser.parse_path_inner(style, None))
                    .unwrap_or_else(|_| panic!("plain identifier path failed full-parser replay"));
            let same = oracle::same_continuation(Site::Path, self, &full_parser)
                && same_path(&path, &full_path)
                && self.token == full_parser.token
                && self.prev_token == full_parser.prev_token
                && self.token_spacing == full_parser.token_spacing
                && self.num_bump_calls == full_parser.num_bump_calls
                && self.expected_token_types.iter().eq(full_parser.expected_token_types.iter());
            if same {
                oracle::record_shadow_ok(Site::Path);
            } else {
                oracle::record_shadow_mismatch(Site::Path);
                *self = full_parser;
                return Some(full_path);
            }
        }

        Some(path)
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.span == right.span
        && left.segments.len() == right.segments.len()
        && left.segments.iter().zip(right.segments.iter()).all(|(left, right)| {
            left.ident.name == right.ident.name
                && left.ident.span == right.ident.span
                && left.args.is_none()
                && right.args.is_none()
        })
}

fn is_plain_path_ident(token: &Token, ident: Ident, raw: IdentIsRaw) -> bool {
    matches!(token.kind, TokenKind::Ident(..))
        && (raw == IdentIsRaw::Yes || !ident.is_reserved())
        && !token.is_bool_lit()
}

fn final_expectation_mask(style: PathStyle) -> u128 {
    let path_sep = 1u128 << crate::rustc_parse::parser::TokenType::PathSep as u32;
    if style == PathStyle::Type {
        path_sep
            | (1u128 << crate::rustc_parse::parser::TokenType::Lt as u32)
            | (1u128 << crate::rustc_parse::parser::TokenType::OpenParen as u32)
    } else {
        path_sep
    }
}
