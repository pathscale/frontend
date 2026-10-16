//! Symbols live in `frontend_symbol`, below this crate, so the lexer can intern while it scans.
//! This module re-exports them under their historical paths and keeps what needs spans:
//! `Ident` and its printers, the stable hashing of symbols, and the symbol helpers that build
//! an `Ident` or search by edit distance ([`SymbolExt`]).

// Explicit allocation imports keep this module compatible with the crate's no_std prelude.
use alloc::string::{String, ToString};

use core::fmt;
use core::hash::{Hash, Hasher};

use crate::rustc_data_structures::stable_hash::{
    StableCompare, StableHash, StableHashCtxt, StableHasher,
};
use rustc_macros::{Decodable, Encodable, StableHash};

use crate::rustc_span::edit_distance::find_best_match_for_name;
use crate::rustc_span::{DUMMY_SP, Span};

pub use frontend_symbol::{
    ByteSymbol, Interner, PREDEFINED_SYMBOLS_COUNT, STDLIB_STABLE_CRATES, Symbol, kw, sym,
    sym_generated, used_keywords,
};

#[cfg(test)]
mod tests;

/// The `Symbol` helpers that need this crate: an `Ident` to print with, or edit distance.
pub trait SymbolExt {
    /// This method is supposed to be used in error messages, so it's expected to be
    /// identical to printing the original identifier token written in source code
    /// (`token_to_string`, `Ident::to_string`), except that symbols don't keep the rawness flag
    /// or edition, so we have to guess the rawness using the global edition.
    fn to_ident_string(self) -> String;

    /// Checks if `self` is similar to any symbol in `candidates`.
    ///
    /// The returned boolean represents whether the candidate is the same symbol with a different
    /// casing.
    ///
    /// All the candidates are assumed to be lowercase.
    fn find_similar(self, candidates: &[Symbol]) -> Option<(Symbol, bool)>;
}

impl SymbolExt for Symbol {
    fn to_ident_string(self) -> String {
        // Avoid creating an empty identifier, because that asserts in debug builds.
        if self == sym::empty { String::new() } else { Ident::with_dummy_span(self).to_string() }
    }

    fn find_similar(self, candidates: &[Symbol]) -> Option<(Symbol, /* is incorrect case */ bool)> {
        let lowercase = self.as_str().to_lowercase();
        let lowercase_sym = Symbol::intern(&lowercase);
        if candidates.contains(&lowercase_sym) {
            Some((lowercase_sym, true))
        } else {
            find_best_match_for_name(candidates, self, None).map(|similar_sym| (similar_sym, false))
        }
    }
}

impl StableHash for Symbol {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.as_str().stable_hash(hcx, hasher);
    }
}

impl StableCompare for Symbol {
    const CAN_USE_UNSTABLE_SORT: bool = true;

    fn stable_cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl StableHash for ByteSymbol {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.as_byte_str().stable_hash(hcx, hasher);
    }
}

#[derive(Copy, Clone, Eq, StableHash, Encodable, Decodable)]
pub struct Ident {
    /// `name` should never be the empty symbol. If you are considering that,
    /// you are probably conflating "empty identifier with "no identifier" and
    /// you should use `Option<Ident>` instead.
    /// Trying to construct an `Ident` with an empty name will trigger debug assertions.
    pub name: Symbol,
    pub span: Span,
}

impl Ident {
    #[inline]
    /// Constructs a new identifier from a symbol and a span.
    pub fn new(name: Symbol, span: Span) -> Ident {
        debug_assert_ne!(name, sym::empty);
        Ident { name, span }
    }

    /// Constructs a new identifier with a dummy span.
    #[inline]
    pub fn with_dummy_span(name: Symbol) -> Ident {
        Ident::new(name, DUMMY_SP)
    }

    // For dummy identifiers that are never used and absolutely must be
    // present. Note that this does *not* use the empty symbol; `sym::dummy`
    // makes it clear that it's intended as a dummy value, and is more likely
    // to be detected if it accidentally does get used.
    #[inline]
    pub fn dummy() -> Ident {
        Ident::with_dummy_span(sym::dummy)
    }

    /// Maps a string to an identifier with a dummy span.
    #[expect(
        clippy::should_implement_trait,
        reason = "constructs an infallible dummy-span identifier rather than parsing a string"
    )]
    pub fn from_str(string: &str) -> Ident {
        Ident::with_dummy_span(Symbol::intern(string))
    }

    /// Maps a string and a span to an identifier.
    pub fn from_str_and_span(string: &str, span: Span) -> Ident {
        Ident::new(Symbol::intern(string), span)
    }

    /// Replaces `lo` and `hi` with those from `span`, but keep hygiene context.
    pub fn with_span_pos(self, span: Span) -> Ident {
        Ident::new(self.name, span.with_ctxt(self.span.ctxt()))
    }

    /// Creates a new ident with the same span and name with leading quote removed, if any.
    /// Calling it on a `'` ident will return an empty ident, which triggers debug assertions.
    pub fn without_first_quote(self) -> Ident {
        self.as_str()
            .strip_prefix('\'')
            .map_or(self, |name| Ident::new(Symbol::intern(name), self.span))
    }

    /// "Normalize" ident for use in comparisons using "item hygiene".
    /// Identifiers with same string value become same if they came from the same macro 2.0 macro
    /// (e.g., `macro` item, but not `macro_rules` item) and stay different if they came from
    /// different macro 2.0 macros.
    /// Technically, this operation strips all non-opaque marks from ident's syntactic context.
    pub fn normalize_to_macros_2_0(self) -> Ident {
        Ident::new(self.name, self.span.normalize_to_macros_2_0())
    }

    /// "Normalize" ident for use in comparisons using "local variable hygiene".
    /// Identifiers with same string value become same if they came from the same non-transparent
    /// macro (e.g., `macro` or `macro_rules!` items) and stay different if they came from different
    /// non-transparent macros.
    /// Technically, this operation strips all transparent marks from ident's syntactic context.
    #[inline]
    pub fn normalize_to_macro_rules(self) -> Ident {
        Ident::new(self.name, self.span.normalize_to_macro_rules())
    }

    /// Access the underlying string. Takes no lock: the interner's index to string table is
    /// read without it (see `SymbolStrs`).
    ///
    /// Note that the lifetime of the return value is a lie. See
    /// `Symbol::as_str()` for details.
    pub fn as_str(&self) -> &str {
        self.name.as_str()
    }
}

impl PartialEq for Ident {
    #[inline]
    fn eq(&self, rhs: &Self) -> bool {
        self.name == rhs.name && self.span.eq_ctxt(rhs.span)
    }
}

impl Hash for Ident {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.span.ctxt().hash(state);
    }
}

impl fmt::Debug for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)?;
        fmt::Debug::fmt(&self.span.ctxt(), f)
    }
}

/// This implementation is supposed to be used in error messages, so it's expected to be identical
/// to printing the original identifier token written in source code (`token_to_string`),
/// except that AST identifiers don't keep the rawness flag, so we have to guess it.
impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&IdentPrinter::new(self.name, self.guess_print_mode(), None), f)
    }
}

pub enum IdentPrintMode {
    Normal,
    RawIdent,
    RawLifetime,
}

/// The most general type to print identifiers.
///
/// AST pretty-printer is used as a fallback for turning AST structures into token streams for
/// proc macros. Additionally, proc macros may stringify their input and expect it survive the
/// stringification (especially true for proc macro derives written between Rust 1.15 and 1.30).
/// So we need to somehow pretty-print `$crate` in a way preserving at least some of its
/// hygiene data, most importantly name of the crate it refers to.
/// As a result we print `$crate` as `crate` if it refers to the local crate
/// and as `::other_crate_name` if it refers to some other crate.
/// Note, that this is only done if the ident token is printed from inside of AST pretty-printing,
/// but not otherwise. Pretty-printing is the only way for proc macros to discover token contents,
/// so we should not perform this lossy conversion if the top level call to the pretty-printer was
/// done for a token stream or a single token.
pub struct IdentPrinter {
    symbol: Symbol,
    mode: IdentPrintMode,
    /// Span used for retrieving the crate name to which `$crate` refers to,
    /// if this field is `None` then the `$crate` conversion doesn't happen.
    convert_dollar_crate: Option<Span>,
}

impl IdentPrinter {
    /// The most general `IdentPrinter` constructor. Do not use this.
    pub fn new(
        symbol: Symbol,
        mode: IdentPrintMode,
        convert_dollar_crate: Option<Span>,
    ) -> IdentPrinter {
        IdentPrinter { symbol, mode, convert_dollar_crate }
    }

    /// This implementation is supposed to be used when printing identifiers
    /// as a part of pretty-printing for larger AST pieces.
    /// Do not use this either.
    pub fn for_ast_ident(ident: Ident, mode: IdentPrintMode) -> IdentPrinter {
        IdentPrinter::new(ident.name, mode, Some(ident.span))
    }
}

impl fmt::Display for IdentPrinter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.mode {
            IdentPrintMode::Normal
                if self.symbol == kw::DollarCrate
                    && let Some(span) = self.convert_dollar_crate =>
            {
                let converted = span.ctxt().dollar_crate_name();
                if !converted.is_path_segment_keyword() {
                    f.write_str("::")?;
                }
                converted
            }
            IdentPrintMode::Normal => self.symbol,
            IdentPrintMode::RawIdent => {
                f.write_str("r#")?;
                self.symbol
            }
            IdentPrintMode::RawLifetime => {
                f.write_str("'r#")?;
                let s = self
                    .symbol
                    .as_str()
                    .strip_prefix("'")
                    .expect("only lifetime idents should be passed with RawLifetime mode");
                Symbol::intern(s)
            }
        };
        s.fmt(f)
    }
}

/// A newtype around `Ident` that calls [Ident::normalize_to_macro_rules] on
/// construction for "local variable hygiene" comparisons.
///
/// Use this type when you need to compare identifiers according to macro_rules hygiene.
/// This ensures compile-time safety and avoids manual normalization calls.
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct MacroRulesNormalizedIdent(Ident);

impl MacroRulesNormalizedIdent {
    #[inline]
    pub fn new(ident: Ident) -> Self {
        MacroRulesNormalizedIdent(ident.normalize_to_macro_rules())
    }

    pub fn symbol(&self) -> Symbol {
        self.0.name
    }

    pub fn ident(&self) -> Ident {
        self.0
    }
}

impl fmt::Debug for MacroRulesNormalizedIdent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for MacroRulesNormalizedIdent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Ident {
    /// Returns `true` for reserved identifiers used internally for elided lifetimes,
    /// unnamed method parameters, crate root module, error recovery etc.
    pub fn is_special(self) -> bool {
        self.name.is_special()
    }

    /// Returns `true` if the token is a keyword used in the language.
    pub fn is_used_keyword(self) -> bool {
        // Note: `span.edition()` is relatively expensive, don't call it unless necessary.
        self.name.is_used_keyword_always()
            || self.name.is_used_keyword_conditional(|| self.span.edition())
    }

    /// Returns `true` if the token is a keyword reserved for possible future use.
    pub fn is_unused_keyword(self) -> bool {
        // Note: `span.edition()` is relatively expensive, don't call it unless necessary.
        self.name.is_unused_keyword_always()
            || self.name.is_unused_keyword_conditional(|| self.span.edition())
    }

    /// Returns `true` if the token is either a special identifier or a keyword.
    #[inline]
    pub fn is_reserved(self) -> bool {
        // Note: `span.edition()` is relatively expensive, don't call it unless necessary.
        self.name.is_reserved(|| self.span.edition())
    }

    /// A keyword or reserved identifier that can be used as a path segment.
    pub fn is_path_segment_keyword(self) -> bool {
        self.name.is_path_segment_keyword()
    }

    /// We see this identifier in a normal identifier position, like variable name or a type.
    /// How was it written originally? Did it use the raw form? Let's try to guess.
    pub fn is_raw_guess(self) -> bool {
        self.name.can_be_raw() && self.is_reserved()
    }

    /// Given the name of a lifetime without the first quote (`'`),
    /// returns whether the lifetime name is reserved (therefore invalid)
    pub fn is_reserved_lifetime(self) -> bool {
        self.is_reserved() && ![kw::Underscore, kw::Static].contains(&self.name)
    }

    pub fn is_raw_lifetime_guess(self) -> bool {
        // Check that the name isn't just a single quote.
        // `self.without_first_quote()` would return empty ident, which triggers debug assert.
        if self.name.as_str() == "'" {
            return false;
        }
        let ident_without_apostrophe = self.without_first_quote();
        ident_without_apostrophe.name != self.name
            && ident_without_apostrophe.name.can_be_raw()
            && ident_without_apostrophe.is_reserved_lifetime()
    }

    pub fn guess_print_mode(self) -> IdentPrintMode {
        if self.is_raw_lifetime_guess() {
            IdentPrintMode::RawLifetime
        } else if self.is_raw_guess() {
            IdentPrintMode::RawIdent
        } else {
            IdentPrintMode::Normal
        }
    }

    /// Whether this would be the identifier for a tuple field like `self.0`, as
    /// opposed to a named field like `self.thing`.
    pub fn is_numeric(self) -> bool {
        self.as_str().bytes().all(|b| b.is_ascii_digit())
    }
}
