use crate::rustc_ast::token::TokenKind;
use crate::rustc_span::symbol::{Symbol, kw, sym};
use alloc::string::String;
use alloc::string::ToString;

/// Used in "expected"/"expected one of" error messages. Tokens are added here
/// as necessary. Tokens with values (e.g. literals, identifiers) are
/// represented by a single variant (e.g. `Literal`, `Ident`).
///
/// It's an awkward representation, but it's important for performance. It's a
/// C-style parameterless enum so that `TokenTypeSet` can be a bitset. This is
/// important because `Parser::expected_token_types` is very hot. `TokenType`
/// used to have variants with parameters (e.g. all the keywords were in a
/// single `Keyword` variant with a `Symbol` parameter) and
/// `Parser::expected_token_types` was a `Vec<TokenType>` which was much slower
/// to manipulate.
///
/// We really want to keep the number of variants to 128 or fewer, so that
/// `TokenTypeSet` can be implemented with a `u128`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TokenType {
    // Expression-operator symbols
    Eq,
    Lt,
    Le,
    EqEq,
    Gt,
    AndAnd,
    OrOr,
    Bang,
    Tilde,

    // BinOps
    Plus,
    Minus,
    Star,
    And,
    Or,

    // Structural symbols
    At,
    Dot,
    DotDot,
    DotDotDot,
    DotDotEq,
    Comma,
    Semi,
    Colon,
    PathSep,
    RArrow,
    FatArrow,
    Pound,
    Question,
    OpenParen,
    CloseParen,
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    Eof,

    // Token types with some details elided.
    /// Any operator.
    Operator,
    /// Any identifier token.
    Ident,
    /// Any lifetime token.
    Lifetime,
    /// Any token that can start a path.
    Path,
    /// Any token that can start a type.
    Type,
    /// Any token that can start a const expression.
    Const,

    // Keywords
    // tidy-alphabetical-start
    KwAs,
    KwAsync,
    KwAuto,
    KwAwait,
    KwBecome,
    KwBox,
    KwBreak,
    KwCatch,
    KwConst,
    KwContinue,
    KwContractEnsures,
    KwContractRequires,
    KwCrate,
    KwDefault,
    KwDyn,
    KwElse,
    KwEnum,
    KwExtern,
    KwFinal,
    KwFn,
    KwFor,
    KwGen,
    KwIf,
    KwImpl,
    KwIn,
    KwLet,
    KwLoop,
    KwMacro,
    KwMacroRules,
    KwMatch,
    KwMod,
    KwMove,
    KwMut,
    KwPin,
    KwPub,
    KwRaw,
    KwRef,
    KwReturn,
    KwReuse,
    KwSafe,
    KwSelfUpper,
    KwStatic,
    KwStruct,
    KwSuper,
    KwTrait,
    KwTry,
    KwType,
    KwUnderscore,
    KwUnsafe,
    KwUse,
    KwWhere,
    KwWhile,
    KwYield,
    // tidy-alphabetical-end

    // Keyword-like symbols.
    // tidy-alphabetical-start
    SymAttSyntax,
    SymBikeshed,
    SymClobberAbi,
    SymInlateout,
    SymInout,
    SymIs,
    SymLabel,
    SymLateout,
    SymMayUnwind,
    SymNomem,
    SymNoreturn,
    SymNostack,
    SymNull,
    SymOptions,
    SymOut,
    SymPin,
    SymPreservesFlags,
    SymPure,
    SymReadonly,
    SymSym,
    // tidy-alphabetical-end
}

// Macro to avoid repetitive boilerplate code.
macro_rules! from_u32_match {
    ($val:ident; $($tok:ident,)+) => {
        // A more obvious formulation would be `0 => TokenType::Eq`. But
        // this formulation with the guard lets us avoid specifying a
        // specific integer for each variant.
        match $val {
            $(
                t if t == TokenType::$tok as u32 => TokenType::$tok,
            )+
            _ => panic!("unhandled value: {}", $val),
        }
    };
}

impl TokenType {
    fn from_u32(val: u32) -> TokenType {
        from_u32_match! { val;
            Eq,
            Lt,
            Le,
            EqEq,
            Gt,
            AndAnd,
            OrOr,
            Bang,
            Tilde,

            Plus,
            Minus,
            Star,
            And,
            Or,

            At,
            Dot,
            DotDot,
            DotDotDot,
            DotDotEq,
            Comma,
            Semi,
            Colon,
            PathSep,
            RArrow,
            FatArrow,
            Pound,
            Question,
            OpenParen,
            CloseParen,
            OpenBrace,
            CloseBrace,
            OpenBracket,
            CloseBracket,
            Eof,

            Operator,
            Ident,
            Lifetime,
            Path,
            Type,
            Const,

            KwAs,
            KwAsync,
            KwAuto,
            KwAwait,
            KwBecome,
            KwBox,
            KwBreak,
            KwCatch,
            KwConst,
            KwContinue,
            KwContractEnsures,
            KwContractRequires,
            KwCrate,
            KwDefault,
            KwDyn,
            KwElse,
            KwEnum,
            KwExtern,
            KwFn,
            KwFor,
            KwFinal,
            KwGen,
            KwIf,
            KwImpl,
            KwIn,
            KwLet,
            KwLoop,
            KwMacro,
            KwMacroRules,
            KwMatch,
            KwMod,
            KwMove,
            KwMut,
            KwPin,
            KwPub,
            KwRaw,
            KwRef,
            KwReturn,
            KwReuse,
            KwSafe,
            KwSelfUpper,
            KwStatic,
            KwStruct,
            KwSuper,
            KwTrait,
            KwTry,
            KwType,
            KwUnderscore,
            KwUnsafe,
            KwUse,
            KwWhere,
            KwWhile,
            KwYield,

            SymAttSyntax,
            SymClobberAbi,
            SymInlateout,
            SymInout,
            SymIs,
            SymLabel,
            SymLateout,
            SymMayUnwind,
            SymNomem,
            SymNoreturn,
            SymNostack,
            SymNull,
            SymOptions,
            SymOut,
            SymPreservesFlags,
            SymPure,
            SymReadonly,
            SymSym,
        }
    }

    pub(super) fn is_keyword(&self) -> Option<Symbol> {
        match self {
            TokenType::KwAs => Some(kw::As),
            TokenType::KwAsync => Some(kw::Async),
            TokenType::KwAuto => Some(kw::Auto),
            TokenType::KwAwait => Some(kw::Await),
            TokenType::KwBecome => Some(kw::Become),
            TokenType::KwBox => Some(kw::Box),
            TokenType::KwBreak => Some(kw::Break),
            TokenType::KwCatch => Some(kw::Catch),
            TokenType::KwConst => Some(kw::Const),
            TokenType::KwContinue => Some(kw::Continue),
            TokenType::KwContractEnsures => Some(kw::ContractEnsures),
            TokenType::KwContractRequires => Some(kw::ContractRequires),
            TokenType::KwCrate => Some(kw::Crate),
            TokenType::KwDefault => Some(kw::Default),
            TokenType::KwDyn => Some(kw::Dyn),
            TokenType::KwElse => Some(kw::Else),
            TokenType::KwEnum => Some(kw::Enum),
            TokenType::KwExtern => Some(kw::Extern),
            TokenType::KwFn => Some(kw::Fn),
            TokenType::KwFor => Some(kw::For),
            TokenType::KwFinal => Some(kw::Final),
            TokenType::KwGen => Some(kw::Gen),
            TokenType::KwIf => Some(kw::If),
            TokenType::KwImpl => Some(kw::Impl),
            TokenType::KwIn => Some(kw::In),
            TokenType::KwLet => Some(kw::Let),
            TokenType::KwLoop => Some(kw::Loop),
            TokenType::KwMacroRules => Some(kw::MacroRules),
            TokenType::KwMacro => Some(kw::Macro),
            TokenType::KwMatch => Some(kw::Match),
            TokenType::KwMod => Some(kw::Mod),
            TokenType::KwMove => Some(kw::Move),
            TokenType::KwMut => Some(kw::Mut),
            TokenType::KwPin => Some(kw::Pin),
            TokenType::KwPub => Some(kw::Pub),
            TokenType::KwRaw => Some(kw::Raw),
            TokenType::KwRef => Some(kw::Ref),
            TokenType::KwReturn => Some(kw::Return),
            TokenType::KwReuse => Some(kw::Reuse),
            TokenType::KwSafe => Some(kw::Safe),
            TokenType::KwSelfUpper => Some(kw::SelfUpper),
            TokenType::KwStatic => Some(kw::Static),
            TokenType::KwStruct => Some(kw::Struct),
            TokenType::KwSuper => Some(kw::Super),
            TokenType::KwTrait => Some(kw::Trait),
            TokenType::KwTry => Some(kw::Try),
            TokenType::KwType => Some(kw::Type),
            TokenType::KwUnderscore => Some(kw::Underscore),
            TokenType::KwUnsafe => Some(kw::Unsafe),
            TokenType::KwUse => Some(kw::Use),
            TokenType::KwWhere => Some(kw::Where),
            TokenType::KwWhile => Some(kw::While),
            TokenType::KwYield => Some(kw::Yield),

            TokenType::SymAttSyntax => Some(sym::att_syntax),
            TokenType::SymClobberAbi => Some(sym::clobber_abi),
            TokenType::SymInlateout => Some(sym::inlateout),
            TokenType::SymInout => Some(sym::inout),
            TokenType::SymIs => Some(sym::is),
            TokenType::SymLabel => Some(sym::label),
            TokenType::SymLateout => Some(sym::lateout),
            TokenType::SymMayUnwind => Some(sym::may_unwind),
            TokenType::SymNomem => Some(sym::nomem),
            TokenType::SymNoreturn => Some(sym::noreturn),
            TokenType::SymNostack => Some(sym::nostack),
            TokenType::SymNull => Some(sym::null),
            TokenType::SymOptions => Some(sym::options),
            TokenType::SymOut => Some(sym::out),
            TokenType::SymPreservesFlags => Some(sym::preserves_flags),
            TokenType::SymPure => Some(sym::pure),
            TokenType::SymReadonly => Some(sym::readonly),
            TokenType::SymSym => Some(sym::sym),
            _ => None,
        }
    }

    // The output should be the same as that produced by
    // `crate::rustc_ast_pretty::pprust::token_to_string`.
    #[expect(
        clippy::wrong_self_convention,
        clippy::inherent_to_string,
        reason = "preserves parser diagnostic wording through its existing on-demand string helper"
    )]
    pub(super) fn to_string(&self) -> String {
        match self {
            TokenType::Eq => "`=`",
            TokenType::Lt => "`<`",
            TokenType::Le => "`<=`",
            TokenType::EqEq => "`==`",
            TokenType::Gt => "`>`",
            TokenType::AndAnd => "`&&`",
            TokenType::OrOr => "`||`",
            TokenType::Bang => "`!`",
            TokenType::Tilde => "`~`",

            TokenType::Plus => "`+`",
            TokenType::Minus => "`-`",
            TokenType::Star => "`*`",
            TokenType::And => "`&`",
            TokenType::Or => "`|`",

            TokenType::At => "`@`",
            TokenType::Dot => "`.`",
            TokenType::DotDot => "`..`",
            TokenType::DotDotDot => "`...`",
            TokenType::DotDotEq => "`..=`",
            TokenType::Comma => "`,`",
            TokenType::Semi => "`;`",
            TokenType::Colon => "`:`",
            TokenType::PathSep => "`::`",
            TokenType::RArrow => "`->`",
            TokenType::FatArrow => "`=>`",
            TokenType::Pound => "`#`",
            TokenType::Question => "`?`",
            TokenType::OpenParen => "`(`",
            TokenType::CloseParen => "`)`",
            TokenType::OpenBrace => "`{`",
            TokenType::CloseBrace => "`}`",
            TokenType::OpenBracket => "`[`",
            TokenType::CloseBracket => "`]`",
            TokenType::Eof => "<eof>",

            TokenType::Operator => "an operator",
            TokenType::Ident => "identifier",
            TokenType::Lifetime => "lifetime",
            TokenType::Path => "path",
            TokenType::Type => "type",
            TokenType::Const => "a const expression",

            _ => return format!("`{}`", self.is_keyword().unwrap()),
        }
        .to_string()
    }
}

/// Used by various `Parser` methods such as `check` and `eat`. The first field
/// is always by used those methods. The second field is only used when the
/// first field doesn't match.
#[derive(Clone, Copy, Debug)]
pub struct ExpTokenPair {
    pub tok: TokenKind,
    pub token_type: TokenType,
}

/// Used by various `Parser` methods such as `check_keyword` and `eat_keyword`.
/// The first field is always used by those methods. The second field is only
/// used when the first field doesn't match.
#[derive(Clone, Copy)]
pub struct ExpKeywordPair {
    pub kw: Symbol,
    pub token_type: TokenType,
}

// Gets a statically-known `ExpTokenPair` pair (for non-keywords) or
// `ExpKeywordPair` (for keywords), as used with various `check`/`expect`
// methods in `Parser`.
//
// The name is short because it's used a lot.
#[macro_export]
macro_rules! exp {
    // `ExpTokenPair` helper rules.
    (@tok, $tok:ident) => {
        $crate::rustc_parse::parser::token_type::ExpTokenPair {
            tok: $crate::rustc_ast::token::$tok,
            token_type: $crate::rustc_parse::parser::token_type::TokenType::$tok
        }
    };

    // `ExpKeywordPair` helper rules.
    (@kw, $kw:ident, $token_type:ident) => {
        $crate::rustc_parse::parser::token_type::ExpKeywordPair {
            kw: $crate::rustc_span::symbol::kw::$kw,
            token_type: $crate::rustc_parse::parser::token_type::TokenType::$token_type,
        }
    };
    (@sym, $kw:ident, $token_type:ident) => {
        $crate::rustc_parse::parser::token_type::ExpKeywordPair {
            kw: $crate::rustc_span::symbol::sym::$kw,
            token_type: $crate::rustc_parse::parser::token_type::TokenType::$token_type,
        }
    };

    (Eq)             => { $crate::exp!(@tok, Eq) };
    (Lt)             => { $crate::exp!(@tok, Lt) };
    (Le)             => { $crate::exp!(@tok, Le) };
    (EqEq)           => { $crate::exp!(@tok, EqEq) };
    (Gt)             => { $crate::exp!(@tok, Gt) };
    (AndAnd)         => { $crate::exp!(@tok, AndAnd) };
    (OrOr)           => { $crate::exp!(@tok, OrOr) };
    (Bang)           => { $crate::exp!(@tok, Bang) };
    (Tilde)          => { $crate::exp!(@tok, Tilde) };
    (Plus)           => { $crate::exp!(@tok, Plus) };
    (Minus)          => { $crate::exp!(@tok, Minus) };
    (Star)           => { $crate::exp!(@tok, Star) };
    (And)            => { $crate::exp!(@tok, And) };
    (Or)             => { $crate::exp!(@tok, Or) };
    (At)             => { $crate::exp!(@tok, At) };
    (Dot)            => { $crate::exp!(@tok, Dot) };
    (DotDot)         => { $crate::exp!(@tok, DotDot) };
    (DotDotDot)      => { $crate::exp!(@tok, DotDotDot) };
    (DotDotEq)       => { $crate::exp!(@tok, DotDotEq) };
    (Comma)          => { $crate::exp!(@tok, Comma) };
    (Semi)           => { $crate::exp!(@tok, Semi) };
    (Colon)          => { $crate::exp!(@tok, Colon) };
    (PathSep)        => { $crate::exp!(@tok, PathSep) };
    (RArrow)         => { $crate::exp!(@tok, RArrow) };
    (FatArrow)       => { $crate::exp!(@tok, FatArrow) };
    (Pound)          => { $crate::exp!(@tok, Pound) };
    (Question)       => { $crate::exp!(@tok, Question) };
    (Eof)            => { $crate::exp!(@tok, Eof) };

    (OpenParen)      => { $crate::exp!(@tok, OpenParen) };
    (OpenBrace)      => { $crate::exp!(@tok, OpenBrace) };
    (OpenBracket)    => { $crate::exp!(@tok, OpenBracket) };
    (CloseParen)     => { $crate::exp!(@tok, CloseParen) };
    (CloseBrace)     => { $crate::exp!(@tok, CloseBrace) };
    (CloseBracket)   => { $crate::exp!(@tok, CloseBracket) };

    (As)             => { $crate::exp!(@kw, As,         KwAs) };
    (Async)          => { $crate::exp!(@kw, Async,      KwAsync) };
    (Auto)           => { $crate::exp!(@kw, Auto,       KwAuto) };
    (Await)          => { $crate::exp!(@kw, Await,      KwAwait) };
    (Become)         => { $crate::exp!(@kw, Become,     KwBecome) };
    (Box)            => { $crate::exp!(@kw, Box,        KwBox) };
    (Break)          => { $crate::exp!(@kw, Break,      KwBreak) };
    (Catch)          => { $crate::exp!(@kw, Catch,      KwCatch) };
    (Const)          => { $crate::exp!(@kw, Const,      KwConst) };
    (Continue)       => { $crate::exp!(@kw, Continue,   KwContinue) };
    (ContractEnsures)  => { $crate::exp!(@kw, ContractEnsures, KwContractEnsures) };
    (ContractRequires) => { $crate::exp!(@kw, ContractRequires, KwContractRequires) };
    (Crate)          => { $crate::exp!(@kw, Crate,      KwCrate) };
    (Default)        => { $crate::exp!(@kw, Default,    KwDefault) };
    (Dyn)            => { $crate::exp!(@kw, Dyn,        KwDyn) };
    (Else)           => { $crate::exp!(@kw, Else,       KwElse) };
    (Enum)           => { $crate::exp!(@kw, Enum,       KwEnum) };
    (Extern)         => { $crate::exp!(@kw, Extern,     KwExtern) };
    (Fn)             => { $crate::exp!(@kw, Fn,         KwFn) };
    (For)            => { $crate::exp!(@kw, For,        KwFor) };
    (Final)          => { $crate::exp!(@kw, Final,      KwFinal) };
    (Gen)            => { $crate::exp!(@kw, Gen,        KwGen) };
    (If)             => { $crate::exp!(@kw, If,         KwIf) };
    (Impl)           => { $crate::exp!(@kw, Impl,       KwImpl) };
    (In)             => { $crate::exp!(@kw, In,         KwIn) };
    (Let)            => { $crate::exp!(@kw, Let,        KwLet) };
    (Loop)           => { $crate::exp!(@kw, Loop,       KwLoop) };
    (Macro)          => { $crate::exp!(@kw, Macro,      KwMacro) };
    (MacroRules)     => { $crate::exp!(@kw, MacroRules, KwMacroRules) };
    (Match)          => { $crate::exp!(@kw, Match,      KwMatch) };
    (Mod)            => { $crate::exp!(@kw, Mod,        KwMod) };
    (Move)           => { $crate::exp!(@kw, Move,       KwMove) };
    (Mut)            => { $crate::exp!(@kw, Mut,        KwMut) };
    (Pin)            => { $crate::exp!(@kw, Pin,        KwMut) };
    (Pub)            => { $crate::exp!(@kw, Pub,        KwPub) };
    (Raw)            => { $crate::exp!(@kw, Raw,        KwRaw) };
    (Ref)            => { $crate::exp!(@kw, Ref,        KwRef) };
    (Return)         => { $crate::exp!(@kw, Return,     KwReturn) };
    (Reuse)          => { $crate::exp!(@kw, Reuse,      KwReuse) };
    (Safe)           => { $crate::exp!(@kw, Safe,       KwSafe) };
    (SelfUpper)      => { $crate::exp!(@kw, SelfUpper,  KwSelfUpper) };
    (Static)         => { $crate::exp!(@kw, Static,     KwStatic) };
    (Struct)         => { $crate::exp!(@kw, Struct,     KwStruct) };
    (Super)          => { $crate::exp!(@kw, Super,      KwSuper) };
    (Trait)          => { $crate::exp!(@kw, Trait,      KwTrait) };
    (Try)            => { $crate::exp!(@kw, Try,        KwTry) };
    (Type)           => { $crate::exp!(@kw, Type,       KwType) };
    (Underscore)     => { $crate::exp!(@kw, Underscore, KwUnderscore) };
    (Unsafe)         => { $crate::exp!(@kw, Unsafe,     KwUnsafe) };
    (Use)            => { $crate::exp!(@kw, Use,        KwUse) };
    (Where)          => { $crate::exp!(@kw, Where,      KwWhere) };
    (While)          => { $crate::exp!(@kw, While,      KwWhile) };
    (Yield)          => { $crate::exp!(@kw, Yield,      KwYield) };

    (AttSyntax)      => { $crate::exp!(@sym, att_syntax,      SymAttSyntax) };
    (Bikeshed)       => { $crate::exp!(@sym, bikeshed,        SymBikeshed) };
    (ClobberAbi)     => { $crate::exp!(@sym, clobber_abi,     SymClobberAbi) };
    (Inlateout)      => { $crate::exp!(@sym, inlateout,       SymInlateout) };
    (Inout)          => { $crate::exp!(@sym, inout,           SymInout) };
    (Is)             => { $crate::exp!(@sym, is,              SymIs) };
    (Label)          => { $crate::exp!(@sym, label,           SymLabel) };
    (Lateout)        => { $crate::exp!(@sym, lateout,         SymLateout) };
    (MayUnwind)      => { $crate::exp!(@sym, may_unwind,      SymMayUnwind) };
    (Nomem)          => { $crate::exp!(@sym, nomem,           SymNomem) };
    (Noreturn)       => { $crate::exp!(@sym, noreturn,        SymNoreturn) };
    (Nostack)        => { $crate::exp!(@sym, nostack,         SymNostack) };
    (Null)           => { $crate::exp!(@sym, null,            SymNull) };
    (Options)        => { $crate::exp!(@sym, options,         SymOptions) };
    (Out)            => { $crate::exp!(@sym, out,             SymOut) };
    (Pin)            => { $crate::exp!(@sym, pin,             SymPin) };
    (PreservesFlags) => { $crate::exp!(@sym, preserves_flags, SymPreservesFlags) };
    (Pure)           => { $crate::exp!(@sym, pure,            SymPure) };
    (Readonly)       => { $crate::exp!(@sym, readonly,        SymReadonly) };
    (Sym)            => { $crate::exp!(@sym, sym,             SymSym) };
}

/// A bitset type designed specifically for `Parser::expected_token_types`,
/// which is very hot. `u128` is the smallest integer that will fit every
/// `TokenType` value.
#[derive(Clone, Copy)]
pub(super) struct TokenTypeSet(u128);

// Keep the deferred mode in the existing word so parser snapshots do not grow.
// A deferred set never records token bits. Its caller must repeat failed parses
// with a normal set before exposing diagnostics.
const DEFERRED_EXPECTATIONS: u128 = 1u128 << 127;
const _: () = assert!((TokenType::SymSym as u32) < 127);

impl TokenTypeSet {
    pub(super) const fn new() -> TokenTypeSet {
        TokenTypeSet(0)
    }

    pub(super) const fn deferred() -> TokenTypeSet {
        TokenTypeSet(DEFERRED_EXPECTATIONS)
    }

    #[inline(always)]
    pub(super) fn is_deferred(&self) -> bool {
        self.0 & DEFERRED_EXPECTATIONS != 0
    }

    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        self.0 == 0
    }

    #[inline(always)]
    pub(super) fn insert(&mut self, token_type: TokenType) {
        if !self.is_deferred() {
            self.0 |= 1u128 << token_type as u32;
        }
    }

    /// Successful checks leave the diagnostic set untouched. Deferred checks
    /// also avoid the runtime shift and store on a failed alternative.
    #[inline(always)]
    pub(super) fn insert_unless(&mut self, present: bool, token_type: TokenType) {
        if !present {
            self.insert(token_type);
        }
    }

    #[inline(always)]
    pub(super) fn clear(&mut self) {
        if !self.is_deferred() {
            self.0 = 0;
        }
    }

    /// Record a set known at the call site with a single union.
    #[inline(always)]
    pub(super) fn insert_mask(&mut self, mask: u128) {
        if !self.is_deferred() {
            self.0 |= mask;
        }
    }

    #[inline]
    pub(super) fn contains(&self, token_type: TokenType) -> bool {
        self.0 & (1u128 << token_type as u32) != 0
    }

    pub(super) fn iter(&self) -> TokenTypeSetIter {
        TokenTypeSetIter(TokenTypeSet(self.0 & !DEFERRED_EXPECTATIONS))
    }
}

// The `TokenTypeSet` is a copy of the set being iterated. It initially holds
// the entire set. Each bit is cleared as it is returned. We have finished once
// it is all zeroes.
pub(super) struct TokenTypeSetIter(TokenTypeSet);

impl Iterator for TokenTypeSetIter {
    type Item = TokenType;

    fn next(&mut self) -> Option<TokenType> {
        let num_bits: u32 = (size_of_val(&self.0.0) * 8) as u32;
        assert_eq!(num_bits, 128);
        let z = self.0.0.trailing_zeros();
        if z == num_bits {
            None
        } else {
            self.0.0 &= !(1 << z); // clear the trailing 1 bit
            Some(TokenType::from_u32(z))
        }
    }
}
