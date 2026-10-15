//! Errors emitted by ty_utils

use crate::rustc_errors::codes::*;
use crate::rustc_middle::ty::{GenericArg, Ty};
use crate::rustc_span::Span;
use rustc_macros::{Diagnostic, Subdiagnostic};

#[derive(Diagnostic)]
#[diag("overflow while checking whether `{$query_ty}` requires drop")]
pub(crate) struct NeedsDropOverflow<'tcx> {
    pub query_ty: Ty<'tcx>,
}

#[derive(Diagnostic)]
#[diag("overly complex generic constant")]
#[help("consider moving this anonymous constant into a `const` function")]
pub(crate) struct GenericConstantTooComplex {
    #[primary_span]
    pub span: Span,
    #[note("this operation may be supported in the future")]
    pub maybe_supported: bool,
    #[subdiagnostic]
    pub sub: GenericConstantTooComplexSub,
}

#[derive(Subdiagnostic)]
pub(crate) enum GenericConstantTooComplexSub {
    #[label("borrowing is not supported in generic constants")]
    Borrow(#[primary_span] Span),
    #[label("dereferencing or taking the address is not supported in generic constants")]
    AddressAndDeref(#[primary_span] Span),
    #[label("array construction is not supported in generic constants")]
    Array(#[primary_span] Span),
    #[label("blocks are not supported in generic constants")]
    Block(#[primary_span] Span),
    #[label("coercing the `never` type is not supported in generic constants")]
    NeverToAny(#[primary_span] Span),
    #[label("tuple construction is not supported in generic constants")]
    Tuple(#[primary_span] Span),
    #[label("indexing is not supported in generic constants")]
    Index(#[primary_span] Span),
    #[label("field access is not supported in generic constants")]
    Field(#[primary_span] Span),
    #[label("const blocks are not supported in generic constants")]
    ConstBlock(#[primary_span] Span),
    #[label("struct/enum construction is not supported in generic constants")]
    Adt(#[primary_span] Span),
    #[label("pointer casts are not allowed in generic constants")]
    Pointer(#[primary_span] Span),
    #[label("coroutine control flow is not allowed in generic constants")]
    Yield(#[primary_span] Span),
    #[label("loops and loop control flow are not supported in generic constants")]
    Loop(#[primary_span] Span),
    #[label("unsupported binary operation in generic constants")]
    Binary(#[primary_span] Span),
    #[label(".use is not allowed in generic constants")]
    ByUse(#[primary_span] Span),
    #[label(
        "unsupported operation in generic constants, short-circuiting operations would imply control flow"
    )]
    LogicalOp(#[primary_span] Span),
    #[label("assignment is not supported in generic constants")]
    Assign(#[primary_span] Span),
    #[label("closures and function keywords are not supported in generic constants")]
    ClosureAndReturn(#[primary_span] Span),
    #[label("control flow is not supported in generic constants")]
    ControlFlow(#[primary_span] Span),
    #[label("assembly is not supported in generic constants")]
    InlineAsm(#[primary_span] Span),
    #[label("unsupported operation in generic constants")]
    Operation(#[primary_span] Span),
}

#[derive(Diagnostic)]
#[diag(
    "monomorphising SIMD type `{$ty}` with a non-primitive-scalar (integer/float/pointer) element type `{$e_ty}`"
)]
pub(crate) struct NonPrimitiveSimdType<'tcx> {
    pub ty: Ty<'tcx>,
    pub e_ty: Ty<'tcx>,
}

#[derive(Diagnostic)]
#[diag("non-defining opaque type use in defining scope")]
pub(crate) struct DuplicateArg<'tcx> {
    pub arg: GenericArg<'tcx>,
    #[primary_span]
    #[label("generic argument `{$arg}` used twice")]
    pub span: Span,
    #[note("for this opaque type")]
    pub opaque_span: Span,
}

#[derive(Diagnostic)]
#[diag("non-defining opaque type use in defining scope", code = E0792)]
pub(crate) struct NotParam<'tcx> {
    pub arg: GenericArg<'tcx>,
    #[primary_span]
    #[label("argument `{$arg}` is not a generic parameter")]
    pub span: Span,
    #[note("for this opaque type")]
    pub opaque_span: Span,
}
