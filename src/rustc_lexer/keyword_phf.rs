//! Collision-free packed ASCII lookups for predefined symbols.
//!
//! Length is part of every key, so zero padding cannot admit trailing NULs.
//! No lookup allocates, interns unknown text, or requires compiler session TLS.

use crate::rustc_span::{Symbol, kw, sym};

/// Pack up to eight ASCII bytes into a zero-padded little-endian word.
#[inline]
pub fn pack_ascii_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.len() > 8 {
        return None;
    }
    let mut padded = [0u8; 8];
    padded[..bytes.len()].copy_from_slice(bytes);
    let packed = u64::from_le_bytes(padded);
    if packed & 0x8080_8080_8080_8080 != 0 {
        return None;
    }
    Some(packed)
}

/// Pack up to sixteen ASCII bytes into a zero-padded little-endian word.
#[inline]
pub fn pack_ascii_u128(bytes: &[u8]) -> Option<u128> {
    if bytes.len() > 16 {
        return None;
    }
    let mut padded = [0u8; 16];
    padded[..bytes.len()].copy_from_slice(bytes);
    let packed = u128::from_le_bytes(padded);
    if packed & 0x8080_8080_8080_8080_8080_8080_8080_8080 != 0 {
        return None;
    }
    Some(packed)
}

// Constants and runtime words use the same byte order on every target.
const fn packed_key(bytes: &[u8]) -> (usize, u128) {
    assert!(bytes.len() <= 16);
    let mut word = 0u128;
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i].is_ascii());
        word |= (bytes[i] as u128) << (i * 8);
        i += 1;
    }
    (bytes.len(), word)
}

#[inline]
fn pack_ident(bytes: &[u8]) -> Option<u128> {
    if bytes.len() <= 8 { pack_ascii_u64(bytes).map(u128::from) } else { pack_ascii_u128(bytes) }
}

// The full packed bytes and length form an injective key. Local constants let
// the match dispatch on word values without hashing or reading the input again.
macro_rules! packed_lookup {
    ($function:ident, $namespace:ident, { $($symbol:ident: $text:literal,)* }) => {
        #[inline]
        #[allow(non_upper_case_globals)]
        fn $function(len: usize, word: u128) -> Option<Symbol> {
            $(const $symbol: (usize, u128) = packed_key($text);)*
            match (len, word) {
                $($symbol => core::option::Option::Some($namespace::$symbol),)*
                _ => core::option::Option::None,
            }
        }
    };
}

packed_lookup!(lookup_keyword, kw, {
    DollarCrate: b"$crate",
    PathRoot: b"{{root}}",
    Underscore: b"_",
    As: b"as",
    Break: b"break",
    Const: b"const",
    Continue: b"continue",
    Crate: b"crate",
    Else: b"else",
    Enum: b"enum",
    Extern: b"extern",
    False: b"false",
    Fn: b"fn",
    For: b"for",
    If: b"if",
    Impl: b"impl",
    In: b"in",
    Let: b"let",
    Loop: b"loop",
    Match: b"match",
    Mod: b"mod",
    Move: b"move",
    Mut: b"mut",
    Pub: b"pub",
    Ref: b"ref",
    Return: b"return",
    SelfLower: b"self",
    SelfUpper: b"Self",
    Static: b"static",
    Struct: b"struct",
    Super: b"super",
    Trait: b"trait",
    True: b"true",
    Type: b"type",
    Unsafe: b"unsafe",
    Use: b"use",
    Where: b"where",
    While: b"while",
    Abstract: b"abstract",
    Become: b"become",
    Box: b"box",
    Do: b"do",
    Final: b"final",
    Macro: b"macro",
    Override: b"override",
    Priv: b"priv",
    Typeof: b"typeof",
    Unsized: b"unsized",
    Virtual: b"virtual",
    Yield: b"yield",
    Async: b"async",
    Await: b"await",
    Dyn: b"dyn",
    Gen: b"gen",
    Try: b"try",
    StaticLifetime: b"'static",
    UnderscoreLifetime: b"'_",
    Auto: b"auto",
    Builtin: b"builtin",
    Catch: b"catch",
    ContractEnsures: b"contract_ensures",
    Default: b"default",
    MacroRules: b"macro_rules",
    Pin: b"pin",
    Raw: b"raw",
    Reuse: b"reuse",
    Safe: b"safe",
    Union: b"union",
    Yeet: b"yeet",
});

/// Classify every entry in the snapshot's Keywords block, including lifetimes.
#[inline]
pub fn classify_keyword_bytes(bytes: &[u8]) -> Option<Symbol> {
    // This weak keyword is seventeen bytes and cannot fit either packed word.
    if bytes.len() > 16 {
        return if bytes == b"contract_requires" { Some(kw::ContractRequires) } else { None };
    }
    lookup_keyword(bytes.len(), pack_ident(bytes)?)
}

/// Classify only the two lifetime keywords, including their apostrophe.
#[inline]
pub fn classify_lifetime_keyword_bytes(bytes: &[u8]) -> Option<Symbol> {
    match bytes {
        b"'static" => Some(kw::StaticLifetime),
        b"'_" => Some(kw::UnderscoreLifetime),
        _ => None,
    }
}

packed_lookup!(lookup_preinterned, sym, {
    u8: b"u8",
    u16: b"u16",
    u32: b"u32",
    u64: b"u64",
    u128: b"u128",
    usize: b"usize",
    i8: b"i8",
    i16: b"i16",
    i32: b"i32",
    i64: b"i64",
    i128: b"i128",
    isize: b"isize",
    f32: b"f32",
    f64: b"f64",
    bool: b"bool",
    char: b"char",
    str: b"str",
    Option: b"Option",
    Some: b"Some",
    None: b"None",
    Result: b"Result",
    Ok: b"Ok",
    Err: b"Err",
    Vec: b"Vec",
    String: b"String",
    Box: b"Box",
    Clone: b"Clone",
    Copy: b"Copy",
    Debug: b"Debug",
    Default: b"Default",
    PartialEq: b"PartialEq",
    Eq: b"Eq",
    Hash: b"Hash",
    Send: b"Send",
    Sync: b"Sync",
    Sized: b"Sized",
    Drop: b"Drop",
    core: b"core",
    alloc: b"alloc",
    std: b"std",
    derive: b"derive",
    cfg: b"cfg",
    cfg_attr: b"cfg_attr",
    allow: b"allow",
    warn: b"warn",
    deny: b"deny",
    inline: b"inline",
    repr: b"repr",
    doc: b"doc",
    test: b"test",
    must_use: b"must_use",
});

/// Classify keywords and common predefined identifiers without interning misses.
#[inline]
pub fn classify_preinterned_ident_bytes(bytes: &[u8]) -> Option<Symbol> {
    if let [digit @ b'0'..=b'9'] = bytes {
        // The range proof keeps integer on its predefined-symbol branch.
        return Some(sym::integer(*digit - b'0'));
    }
    if bytes.len() > 16 {
        return classify_keyword_bytes(bytes);
    }
    let word = pack_ident(bytes)?;
    lookup_keyword(bytes.len(), word).or_else(|| lookup_preinterned(bytes.len(), word))
}

/// Edition-independent membership in the strict/reserved keyword union.
///
/// This includes the three special reserved identifiers. Weak and lifetime
/// keywords are excluded. Keep edition-sensitive parser checks at call sites.
#[inline]
pub fn is_strict_or_reserved_keyword(bytes: &[u8]) -> bool {
    match classify_keyword_bytes(bytes) {
        // The snapshot orders special, strict, reserved and edition keywords
        // contiguously, ending at Try, before lifetimes and weak keywords.
        Some(symbol) => symbol >= kw::DollarCrate && symbol <= kw::Try,
        None => false,
    }
}
