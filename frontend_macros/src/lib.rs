// No `rustc::` tool lints here: stable does not know the `rustc` tool and rejects the attribute.
// Deterministic output, which `potential_query_instability` used to guard, now rests on the
// `FxBuildHasher` choice explained in `Cargo.toml`.

use proc_macro::TokenStream;
use synstructure::decl_derive;

mod current_version;
mod diagnostics;
mod extension;
mod lift;
mod print_attribute;
mod query;
mod serialize;
mod stable_hash;
mod symbols;
mod type_foldable;
mod type_visitable;
mod visitable;

// **Two further proc-macro crates were merged into this one.**
//
// A proc-macro crate cannot be merged into a normal crate: the language compiles it separately,
// for the host. It can be merged into another proc-macro crate, and these three were three crates
// only because upstream builds them through bootstrap where the count does not matter. Published,
// each name is permanent and each is a thing somebody has to depend on.
//
// `rustc_index_macros` supplied `newtype_index!` and `rustc_type_ir_macros` the four `_Generic`
// derives. Their helper code lives in the two modules below; the macro declarations themselves have
// to be at the crate root, because that is where `#[proc_macro]` and `#[proc_macro_derive]` are
// required to be, so those sit with the rest of them further down.
mod index_newtype;
mod type_ir_derives;

// Reads the rust version (e.g. "1.75.0") from the CFG_RELEASE env var and
// produces a `RustcVersion` literal containing that version (e.g.
// `RustcVersion { major: 1, minor: 75, patch: 0 }`).
#[proc_macro]
pub fn current_rustc_version(input: TokenStream) -> TokenStream {
    current_version::current_version(input)
}

#[proc_macro]
pub fn rustc_queries(input: TokenStream) -> TokenStream {
    query::rustc_queries(input)
}

#[proc_macro]
pub fn symbols(input: TokenStream) -> TokenStream {
    symbols::symbols(input.into()).into()
}

/// Turns each comma-separated error-code literal `NNNN` into
/// `pub const ENNNN: crate::rustc_errors::ErrCode = crate::rustc_errors::ErrCode::from_u32(NNNN);`.
///
/// `rustc_errors::codes` did this with `${concat(E, $num)}` (`macro_metavar_expr_concat`, which is
/// unstable). `macro_rules!` has no stable way to build an identifier, and the alternative was
/// rewriting all five hundred entries of `error_codes!` by hand, so this is the one proc macro the
/// port adds. The literal's own text (`0001`) names the constant, exactly as `concat` did.
#[proc_macro]
pub fn error_code_constants(input: TokenStream) -> TokenStream {
    let parser = syn::punctuated::Punctuated::<syn::LitInt, syn::Token![,]>::parse_terminated;
    let codes = syn::parse_macro_input!(input with parser);
    let consts = codes.iter().map(|code| {
        let name = quote::format_ident!("E{}", code.to_string(), span = code.span());
        quote::quote! {
            pub const #name: crate::rustc_errors::ErrCode =
                crate::rustc_errors::ErrCode::from_u32(#code);
        }
    });
    quote::quote! { #(#consts)* }.into()
}

/// Derive an extension trait for a given impl block. The trait name
/// goes into the parenthesized args of the macro, for greppability.
/// For example:
/// ```
/// use rustc_macros::extension;
/// #[extension(pub trait Foo)]
/// impl i32 { fn hello() {} }
/// ```
///
/// expands to:
/// ```
/// pub trait Foo { fn hello(); }
/// impl Foo for i32 { fn hello() {} }
/// ```
#[proc_macro_attribute]
pub fn extension(attr: TokenStream, input: TokenStream) -> TokenStream {
    extension::extension(attr, input)
}

/// Creates a struct type `S` that can be used as an index with `IndexVec` and so on.
///
/// Was `rustc_index_macros::newtype_index`. See the note on the module list above.
///
/// Accepted attributes: `#[stable_hash]`, `#[encodable]`, `#[orderable]`,
/// `#[debug_format = "Foo({})"]`, `#[max = 0xFFFF_FFFD]`, `#[gate_rustc_only]`.
///
/// There is no `#[allow_internal_unstable]` here any more: the attribute is itself nightly-only.
/// It used to let the expansion use `step_trait`, `pattern_types`, `pattern_type_macro` and
/// `structural_match` without the calling crate enabling them, so on stable the expansion has to
/// avoid those features on its own, and it does: the field is a plain `u32` and there is no
/// `Step` impl, so a range of indices does not iterate. See `index_newtype.rs`.
#[proc_macro]
pub fn newtype_index(input: TokenStream) -> TokenStream {
    index_newtype::newtype(input)
}

// The four `_Generic` derives, formerly `rustc_type_ir_macros`. Their bodies are in
// `type_ir_derives`; only the declarations have to be here.
decl_derive!(
    [TypeVisitable_Generic, attributes(type_visitable)] => type_ir_derives::type_visitable_derive
);
decl_derive!(
    [TypeFoldable_Generic, attributes(type_foldable)] => type_ir_derives::type_foldable_derive
);
decl_derive!(
    [Lift_Generic, attributes(lift)] => type_ir_derives::lift_derive
);
/// By default `#[derive(GenericTypeVisitable)]` bounds every field's type, which recurses forever
/// for a type whose field mentions `Self`. `#[generic_type_visitable(bounds(...))]` overrides the
/// bound list for that field, and is empty for the `Self`-only case.
decl_derive!(
    [GenericTypeVisitable, attributes(generic_type_visitable)] =>
        type_ir_derives::customizable_type_visitable_derive
);

decl_derive!(
    [StableHash, attributes(stable_hash)] => stable_hash::stable_hash_derive
);
decl_derive!(
    [StableHash_NoContext, attributes(stable_hash)] => stable_hash::stable_hash_no_context_derive
);

// Encoding and Decoding derives
decl_derive!([Decodable_NoContext] =>
    /// See docs on derive [`Decodable`].
    ///
    /// Derives `Decodable<D> for T where D: Decoder`.
    serialize::decodable_nocontext_derive
);
decl_derive!([Encodable_NoContext] => serialize::encodable_nocontext_derive);
decl_derive!([Decodable] =>
    /// Derives `Decodable<D> for T where D: SpanDecoder`
    ///
    /// # Deriving decoding traits
    ///
    /// > Some shared docs about decoding traits, since this is likely the first trait you find
    ///
    /// The difference between these derives can be subtle!
    /// At a high level, there's the `T: Decodable<D>` trait that says some type `T`
    /// can be decoded using a decoder `D`. There are various decoders!
    /// The different derives place different *trait* bounds on this type `D`.
    ///
    /// Even though this derive, based on its name, seems like the most vanilla one,
    /// it actually places a pretty strict bound on `D`: `SpanDecoder`.
    /// It means that types that derive this can contain spans, among other things,
    /// and still be decoded. The reason this is hard is that at least in metadata,
    /// spans can only be decoded later, once some information from the header
    /// is already decoded to properly deal with spans.
    ///
    /// The hierarchy is roughly:
    ///
    /// - derive [`Decodable_NoContext`] is the most relaxed bounds that could be placed on `D`,
    ///   and is only really suited for structs and enums containing primitive types.
    /// - derive [`BlobDecodable`] may be a better default, than deriving `Decodable`:
    ///   it places fewer requirements on `D`, while still allowing some complex types to be decoded.
    /// - derive [`LazyDecodable`]: Only for types containing `Lazy{Array,Table,Value}`.
    /// - derive [`Decodable`] for structures containing spans. Requires `D: SpanDecoder`
    /// - derive [`TyDecodable`] for types that require access to the `TyCtxt` while decoding.
    ///   For example: arena allocated types.
    serialize::decodable_derive
);
decl_derive!([Encodable] => serialize::encodable_derive);
decl_derive!([TyDecodable] =>
    /// See docs on derive [`Decodable`].
    ///
    /// Derives `Decodable<D> for T where D: TyDecoder`.
    serialize::type_decodable_derive
);
decl_derive!([TyEncodable] => serialize::type_encodable_derive);
decl_derive!([LazyDecodable] =>
    /// See docs on derive [`Decodable`].
    ///
    /// Derives `Decodable<D> for T where D: LazyDecoder`.
    /// This constrains the decoder to be specifically the decoder that can decode
    /// `LazyArray`s, `LazyValue`s amd `LazyTable`s in metadata.
    /// Therefore, we only need this on things containing LazyArray really.
    ///
    /// Most decodable derives mirror an encodable derive.
    /// [`LazyDecodable`] and [`BlobDecodable`] together roughly mirror [`MetadataEncodable`]
    serialize::lazy_decodable_derive
);
decl_derive!([BlobDecodable] =>
    /// See docs on derive [`Decodable`].
    ///
    /// Derives `Decodable<D> for T where D: BlobDecoder`.
    ///
    /// Most decodable derives mirror an encodable derive.
    /// [`LazyDecodable`] and [`BlobDecodable`] together roughly mirror [`MetadataEncodable`]
    serialize::blob_decodable_derive
);
decl_derive!([MetadataEncodable] =>
    /// Most encodable derives mirror a decodable derive.
    /// [`MetadataEncodable`] is roughly mirrored by the combination of [`LazyDecodable`] and [`BlobDecodable`]
    serialize::meta_encodable_derive
);

decl_derive!(
    [TypeFoldable, attributes(type_foldable)] =>
    /// Derives `TypeFoldable` for the annotated `struct` or `enum` (`union` is not supported).
    ///
    /// The fold will produce a value of the same struct or enum variant as the input, with
    /// each field respectively folded using the `TypeFoldable` implementation for its type.
    /// However, if a field of a struct or an enum variant is annotated with
    /// `#[type_foldable(identity)]` then that field will retain its incumbent value (and its
    /// type is not required to implement `TypeFoldable`).
    type_foldable::type_foldable_derive
);
decl_derive!(
    [TypeVisitable, attributes(type_visitable)] =>
    /// Derives `TypeVisitable` for the annotated `struct` or `enum` (`union` is not supported).
    ///
    /// Each field of the struct or enum variant will be visited in definition order, using the
    /// `TypeVisitable` implementation for its type. However, if a field of a struct or an enum
    /// variant is annotated with `#[type_visitable(ignore)]` then that field will not be
    /// visited (and its type is not required to implement `TypeVisitable`).
    type_visitable::type_visitable_derive
);
decl_derive!(
    [Walkable, attributes(visitable)] =>
    /// Derives `Walkable` for the annotated `struct` or `enum` (`union` is not supported).
    ///
    /// Each field of the struct or enum variant will be visited in definition order, using the
    /// `Walkable` implementation for its type. However, if a field of a struct or an enum
    /// variant is annotated with `#[visitable(ignore)]` then that field will not be
    /// visited (and its type is not required to implement `Walkable`).
    visitable::visitable_derive
);
decl_derive!([Lift, attributes(lift)] => lift::lift_derive);
decl_derive!(
    [Diagnostic, attributes(
        // struct and field attributes
        diag,
        help,
        help_once,
        note,
        note_once,
        warning,
        // field attributes
        primary_span,
        label,
        subdiagnostic,
        suggestion,
        suggestion_short,
        suggestion_hidden,
        suggestion_verbose)] =>
        #[doc = "See <https://rustc-dev-guide.rust-lang.org/diagnostics/diagnostic-structs.html#derivediagnostic>"]
        diagnostics::diagnostic_derive
);
decl_derive!(
    [Subdiagnostic, attributes(
        // struct/variant attributes
        label,
        help,
        help_once,
        note,
        note_once,
        warning,
        subdiagnostic,
        suggestion,
        suggestion_short,
        suggestion_hidden,
        suggestion_verbose,
        multipart_suggestion,
        multipart_suggestion_short,
        multipart_suggestion_hidden,
        // field attributes
        primary_span,
        suggestion_part,
        applicability)] => diagnostics::subdiagnostic_derive
);

/// This macro creates a `DiagMessage` from a message template.
/// It should be used in places where a message with arguments is needed, but struct diagnostics
/// are undesired.
///
/// It statically checks that the template parses. It cannot check that the variables it names
/// exist, because there is no struct to check them against: they arrive later, from `.arg(..)`.
#[proc_macro]
pub fn msg(input: TokenStream) -> TokenStream {
    diagnostics::msg_macro(input)
}

decl_derive! {
    [PrintAttribute] =>
    /// Derives `PrintAttribute` for `AttributeKind`.
    /// This macro is pretty specific to `rustc_hir::attrs` and likely not that useful in
    /// other places. It's deriving something close to `Debug` without printing some extraneous
    /// things like spans.
    print_attribute::print_attribute
}
