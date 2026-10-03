//! This module implements declarative macros: old `macro_rules` and the newer
//! `macro`. Declarative macros are also known as "macro by example", and that's
//! why we call this module `mbe`. For external documentation, prefer the
//! official terminology: "declarative macros".

use alloc::string::{String, ToString};
use alloc::vec::Vec;
pub(crate) mod diagnostics;
pub(crate) mod macro_rules;

mod macro_check;
mod macro_parser;
mod metavar_expr;
mod quoted;
mod transcribe;

use metavar_expr::MetaVarExpr;
use crate::rustc_ast::token::{Delimiter, InvisibleOrigin, NonterminalKind, Token, TokenKind};
use crate::rustc_ast::tokenstream::{
    DelimSpacing, DelimSpan, Spacing, TokenStream as AstTokenStream,
    TokenTree as AstTokenTree,
};
use crate::rustc_data_structures::fx::FxHashMap;
use crate::rustc_errors::ErrorGuaranteed;
use crate::rustc_feature::Features;
use crate::rustc_parse::parser::ParseNtResult;
use crate::rustc_session::Session;
use rustc_macros::{Decodable, Encodable};
use crate::rustc_span::edition::Edition;
use crate::rustc_span::hygiene::{LocalExpnId, Transparency};
use crate::rustc_span::{Ident, MacroRulesNormalizedIdent, Span};

/// Contains the sub-token-trees of a "delimited" token tree such as `(a b c)`.
/// The delimiters are not represented explicitly in the `tts` vector.
#[derive(PartialEq, Encodable, Decodable, Debug)]
pub(crate) struct Delimited {
    delim: Delimiter,
    /// FIXME: #67062 has details about why this is sub-optimal.
    tts: Vec<TokenTree>,
}

#[derive(PartialEq, Encodable, Decodable, Debug)]
pub(crate) struct SequenceRepetition {
    /// The sequence of token trees
    tts: Vec<TokenTree>,
    /// The optional separator
    separator: Option<Token>,
    /// Whether the sequence can be repeated zero (*), or one or more times (+)
    kleene: KleeneToken,
    /// The number of `Match`s that appear in the sequence (and subsequences)
    num_captures: usize,
}

#[derive(Clone, PartialEq, Encodable, Decodable, Debug, Copy)]
struct KleeneToken {
    span: Span,
    op: KleeneOp,
}

impl KleeneToken {
    fn new(op: KleeneOp, span: Span) -> KleeneToken {
        KleeneToken { span, op }
    }
}

/// A Kleene-style [repetition operator](https://en.wikipedia.org/wiki/Kleene_star)
/// for token sequences.
#[derive(Clone, PartialEq, Encodable, Decodable, Debug, Copy)]
pub(crate) enum KleeneOp {
    /// Kleene star (`*`) for zero or more repetitions
    ZeroOrMore,
    /// Kleene plus (`+`) for one or more repetitions
    OneOrMore,
    /// Kleene optional (`?`) for zero or one repetitions
    ZeroOrOne,
}

/// Similar to `tokenstream::TokenTree`, except that `Sequence`, `MetaVar`, `MetaVarDecl`, and
/// `MetaVarExpr` are "first-class" token trees. Useful for parsing macros.
#[derive(Debug, PartialEq, Encodable, Decodable)]
pub(crate) enum TokenTree {
    /// A token. Unlike `tokenstream::TokenTree::Token` this lacks a `Spacing`.
    /// See the comments about `Spacing` in the `transcribe` function.
    Token(Token),
    /// A delimited sequence, e.g. `($e:expr)` (RHS) or `{ $e }` (LHS).
    Delimited(DelimSpan, DelimSpacing, Delimited),
    /// A kleene-style repetition sequence, e.g. `$($e:expr)*` (RHS) or `$($e),*` (LHS).
    Sequence(DelimSpan, SequenceRepetition),
    /// e.g., `$var`. The span covers the leading dollar and the ident. (The span within the ident
    /// only covers the ident, e.g. `var`.)
    MetaVar(Span, Ident),
    /// e.g., `$var:expr`. Only appears on the LHS.
    MetaVarDecl {
        span: Span,
        /// Name to bind.
        name: Ident,
        /// The fragment specifier.
        kind: NonterminalKind,
    },
    /// A meta-variable expression inside `${...}`.
    MetaVarExpr(DelimSpan, MetaVarExpr),
}

pub(crate) struct SchemaBinding {
    pub name: String,
    pub values: Vec<AstTokenStream>,
    pub repeated: bool,
}

fn schema_metavar(stream: AstTokenStream, span: Span) -> ParseNtResult {
    ParseNtResult::Tt(AstTokenTree::Delimited(
        DelimSpan::from_single(span),
        DelimSpacing::new(Spacing::Alone, Spacing::Alone),
        Delimiter::Invisible(InvisibleOrigin::ProcMacro),
        stream,
    ))
}

fn bind_schema_metavariables(
    trees: &[TokenTree],
    sequence_depth: usize,
    bindings: &[SchemaBinding],
    interp: &mut FxHashMap<MacroRulesNormalizedIdent, macro_parser::NamedMatch>,
    seen: &mut Vec<String>,
) -> Option<usize> {
    let mut count = 0usize;
    for tree in trees {
        match tree {
            TokenTree::MetaVar(_, ident) => {
                let name = ident.name.as_str();
                let binding = bindings.iter().find(|binding| binding.name == name)?;
                if binding.repeated != (sequence_depth != 0) {
                    return None;
                }
                let value = if sequence_depth != 0 {
                    macro_parser::MatchedSeq(
                        binding
                            .values
                            .iter()
                            .cloned()
                            .map(|stream| macro_parser::MatchedSingle(schema_metavar(stream, ident.span)))
                            .collect(),
                    )
                } else {
                    let [stream] = binding.values.as_slice() else { return None };
                    macro_parser::MatchedSingle(schema_metavar(stream.clone(), ident.span))
                };
                let normalized = MacroRulesNormalizedIdent::new(*ident);
                if !seen.iter().any(|seen| seen == name) {
                    interp.insert(normalized, value);
                    seen.push(name.to_string());
                }
                count += 1;
            }
            TokenTree::Delimited(_, _, delimited) => {
                count += bind_schema_metavariables(
                    &delimited.tts,
                    sequence_depth,
                    bindings,
                    interp,
                    seen,
                )?;
            }
            TokenTree::Sequence(_, sequence) => {
                if sequence_depth != 0 {
                    return None;
                }
                let sequence_count = bind_schema_metavariables(
                    &sequence.tts,
                    sequence_depth + 1,
                    bindings,
                    interp,
                    seen,
                )?;
                if sequence_count == 0 {
                    return None;
                }
                count += sequence_count;
            }
            TokenTree::Token(_) | TokenTree::MetaVarDecl { .. } | TokenTree::MetaVarExpr(..) => {}
        }
    }
    Some(count)
}

pub(crate) fn transcribe_schema(
    sess: &Session,
    rhs: AstTokenStream,
    bindings: &[SchemaBinding],
    features: &Features,
    edition: Edition,
    span: Span,
    transparency: Transparency,
    expansion: LocalExpnId,
) -> Result<Option<AstTokenStream>, ErrorGuaranteed> {
    let rhs_trees = quoted::parse_body(&rhs, sess, features, edition);
    let rhs = Delimited { delim: Delimiter::Brace, tts: rhs_trees };
    let mut interp = FxHashMap::default();
    let mut seen = Vec::new();
    if bind_schema_metavariables(&rhs.tts, 0, bindings, &mut interp, &mut seen).is_none()
        || bindings.iter().any(|binding| !seen.iter().any(|seen| seen == &binding.name))
    {
        return Ok(None);
    }

    transcribe::transcribe(
        &sess.psess,
        &interp,
        &rhs,
        DelimSpan::from_single(span),
        transparency,
        expansion,
    )
    .map(Some)
    .map_err(|error| error.emit())
}

impl TokenTree {
    /// Returns `true` if the given token tree is delimited.
    fn is_delimited(&self) -> bool {
        matches!(*self, TokenTree::Delimited(..))
    }

    /// Returns `true` if the given token tree is a token of the given kind.
    fn is_token(&self, expected_kind: &TokenKind) -> bool {
        match self {
            TokenTree::Token(Token { kind: actual_kind, .. }) => actual_kind == expected_kind,
            _ => false,
        }
    }

    /// Retrieves the `TokenTree`'s span.
    fn span(&self) -> Span {
        match *self {
            TokenTree::Token(Token { span, .. })
            | TokenTree::MetaVar(span, _)
            | TokenTree::MetaVarDecl { span, .. } => span,
            TokenTree::Delimited(span, ..)
            | TokenTree::MetaVarExpr(span, _)
            | TokenTree::Sequence(span, _) => span.entire(),
        }
    }

    fn token(kind: TokenKind, span: Span) -> TokenTree {
        TokenTree::Token(Token::new(kind, span))
    }

    // Used only in diagnostics.
    fn meta_vars(&self, vars: &mut Vec<Ident>) {
        match self {
            Self::Token(_) => {}
            Self::MetaVar(_, ident) => vars.push(*ident),
            Self::MetaVarDecl { name, .. } => vars.push(*name),
            Self::Delimited(_, _, delimited) => {
                for tt in &delimited.tts {
                    tt.meta_vars(vars);
                }
            }
            Self::Sequence(_, sequence) => {
                for tt in &sequence.tts {
                    tt.meta_vars(vars);
                }
            }
            Self::MetaVarExpr(_, _) => {}
        }
    }
}
