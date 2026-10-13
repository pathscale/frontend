//! Owned parser state. Copying a body's start never copies earlier capture metadata.

use super::super::token_type::TokenTypeSet;
use super::super::{CaptureState, Capturing, ClosureSpans, Parser, Recovery, Restrictions};
use crate::rustc_ast as ast;
use crate::rustc_ast::token::Token;
use crate::rustc_ast::tokenstream::{FrameSpare, Spacing, TokenCursor};
use crate::rustc_data_structures::fx::FxHashMap;
use crate::rustc_index::interval::IntervalSet;
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::{ErrorGuaranteed, Span};

fn capture(capturing: Capturing) -> CaptureState {
    CaptureState {
        capturing,
        parser_replacements: ast::ArenaVec::new(),
        inner_attr_parser_ranges: FxHashMap::default(),
        seen_attrs: IntervalSet::new(u32::MAX as usize),
    }
}

macro_rules! parser_state {
    ($($field:ident: $ty:ty),+ $(,)?) => {
        pub(super) struct State {
            pub(super) capture_state: CaptureState,
            $(pub(super) $field: $ty,)+
        }
        impl State {
            pub(super) fn copy(parser: &Parser<'_>) -> Self {
                Self {
                    capture_state: parser.capture_state.clone(),
                    $($field: parser.$field.clone(),)+
                }
            }
            pub(super) fn body(parser: &Parser<'_>) -> Self {
                Self {
                    capture_state: capture(parser.capture_state.capturing),
                    $($field: parser.$field.clone(),)+
                }
            }
            pub(super) fn clone_body(&self) -> Self {
                Self {
                    capture_state: capture(self.capture_state.capturing),
                    $($field: self.$field.clone(),)+
                }
            }
            pub(super) fn take(parser: Parser<'_>) -> Self {
                Self {
                    capture_state: parser.capture_state,
                    $($field: parser.$field,)+
                }
            }
            pub(super) fn parser<'a>(self, psess: &'a ParseSess) -> Parser<'a> {
                Parser {
                    psess,
                    frame_spare: FrameSpare::default(),
                    body_jobs: None,
                    capture_state: self.capture_state,
                    $($field: self.$field,)+
                }
            }
            pub(super) fn install(self, parser: &mut Parser<'_>) {
                $(parser.$field = self.$field;)+
                parser.capture_state = self.capture_state;
                parser.frame_spare = FrameSpare::default();
            }
        }
    };
}

parser_state! {
    token: Token,
    token_spacing: Spacing,
    prev_token: Token,
    capture_cfg: bool,
    restrictions: Restrictions,
    expected_token_types: TokenTypeSet,
    token_cursor: TokenCursor,
    num_bump_calls: u32,
    break_last_token: u32,
    unmatched_angle_bracket_count: u16,
    angle_bracket_nesting: u16,
    parsing_generics: bool,
    last_unexpected_token_span: Option<Span>,
    subparser_name: Option<&'static str>,
    current_closure: Option<ClosureSpans>,
    recovery: Recovery,
    in_fn_body: bool,
    fn_body_missing_semi_guar: Option<ErrorGuaranteed>,
}

impl State {
    pub(super) fn continuation_matches(&self, parser: &Parser<'_>) -> bool {
        // Closure recovery state is cleared at the next statement or expression entry.
        // A different last closure does not change an accepted crate's AST or diagnostics.
        parser.restrictions.bits() == self.restrictions.bits()
            && parser.expected_token_types.is_empty()
            && parser.break_last_token == 0
            && parser.unmatched_angle_bracket_count == self.unmatched_angle_bracket_count
            && parser.angle_bracket_nesting == self.angle_bracket_nesting
            && parser.parsing_generics == self.parsing_generics
            && parser.last_unexpected_token_span == self.last_unexpected_token_span
            && parser.in_fn_body
            && parser.fn_body_missing_semi_guar.is_none()
    }
}
