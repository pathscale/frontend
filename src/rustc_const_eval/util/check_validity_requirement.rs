// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing in
// this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
// `discard_err`/`report_err` and friends: an extension trait now that `InterpResult` is a `Result`.
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_abi::{BackendRepr, FieldsShape, Scalar, Variants};
use crate::rustc_middle::ty::layout::{
    HasTyCtxt, LayoutCx, LayoutError, LayoutOf, TyAndLayout, ValidityRequirement,
};
use crate::rustc_middle::ty::{PseudoCanonicalInput, ScalarInt, Ty, TyCtxt};
use crate::rustc_middle::bug;

/// Determines whether the layout permits raw initialization by transmuting memory into `T`.
/// `init_kind` specifies whether the memory is zero-initialized or filled with `0x01`.
pub fn check_validity_requirement<'tcx>(
    tcx: TyCtxt<'tcx>,
    kind: ValidityRequirement,
    input: PseudoCanonicalInput<'tcx, Ty<'tcx>>,
) -> Result<bool, &'tcx LayoutError<'tcx>> {
    let layout = tcx.layout_of(input)?;

    // There is nothing strict or lax about inhabitedness.
    if kind == ValidityRequirement::Inhabited {
        return Ok(!layout.is_uninhabited());
    }

    if kind == ValidityRequirement::Uninit {
        return Ok(false);
    }

    let layout_cx = LayoutCx::new(tcx, input.typing_env);
    check_validity_requirement_lax(layout, &layout_cx, kind)
}

/// Implements the 'lax' (default) version of the [`check_validity_requirement`] checks; see that
/// function for details.
fn check_validity_requirement_lax<'tcx>(
    this: TyAndLayout<'tcx>,
    cx: &LayoutCx<'tcx>,
    init_kind: ValidityRequirement,
) -> Result<bool, &'tcx LayoutError<'tcx>> {
    let scalar_allows_raw_init = move |s: Scalar| -> bool {
        match init_kind {
            ValidityRequirement::Inhabited => {
                bug!("ValidityRequirement::Inhabited should have been handled above")
            }
            ValidityRequirement::Zero => {
                // The range must contain 0.
                s.valid_range(cx).contains(0)
            }
            ValidityRequirement::UninitMitigated0x01Fill => {
                // The range must include an 0x01-filled buffer.
                let mut val: u128 = 0x01;
                for _ in 1..s.size(cx).bytes() {
                    // For sizes >1, repeat the 0x01.
                    val = (val << 8) | 0x01;
                }
                s.valid_range(cx).contains(val)
            }
            ValidityRequirement::Uninit => {
                bug!("ValidityRequirement::Uninit should have been handled above")
            }
        }
    };

    // Check the ABI.
    let valid = !this.is_uninhabited() // definitely UB if uninhabited
        && match this.backend_repr {
            BackendRepr::Scalar(s) => scalar_allows_raw_init(s),
            BackendRepr::ScalarPair { a: s1, b: s2, b_offset: _ } => {
                scalar_allows_raw_init(s1) && scalar_allows_raw_init(s2)
            }
            BackendRepr::SimdVector { element: s, count: _ } => scalar_allows_raw_init(s),
            BackendRepr::Memory { .. } => true, // Fields are checked below.
            BackendRepr::SimdScalableVector { element, .. } => scalar_allows_raw_init(element),
        };

    if !valid {
        // This is definitely not okay.
        return Ok(false);
    }

    // Special magic check for references and boxes (i.e., special pointer types).
    if let Some(pointee) = this.ty.builtin_deref(false) {
        let pointee = cx.layout_of(pointee)?;
        // We need to ensure that the LLVM attributes `aligned` and `dereferenceable(size)` are satisfied.
        if pointee.align.bytes() > 1 {
            // 0x01-filling is not aligned.
            return Ok(false);
        }
        if pointee.size.bytes() > 0 {
            // A 'fake' integer pointer is not sufficiently dereferenceable.
            return Ok(false);
        }
    }

    // If we have not found an error yet, we need to recursively descend into fields.
    match &this.fields {
        FieldsShape::Primitive | FieldsShape::Union { .. } => {}
        FieldsShape::Array { .. } => {
            // Arrays never have scalar layout in LLVM, so if the array is not actually
            // accessed, there is no LLVM UB -- therefore we can skip this.
        }
        FieldsShape::Arbitrary { offsets, .. } => {
            for idx in 0..offsets.len() {
                if !check_validity_requirement_lax(this.field(cx, idx), cx, init_kind)? {
                    // We found a field that is unhappy with this kind of initialization.
                    return Ok(false);
                }
            }
        }
    }

    match &this.variants {
        Variants::Empty => return Ok(false),
        Variants::Single { .. } => {
            // All fields of this single variant have already been checked above, there is nothing
            // else to do.
        }
        Variants::Multiple { .. } => {
            // We cannot tell LLVM anything about the details of this multi-variant layout, so
            // invalid values "hidden" inside the variant cannot cause LLVM trouble.
        }
    }

    Ok(true)
}

pub(crate) fn validate_scalar_in_layout<'tcx>(
    _tcx: TyCtxt<'tcx>,
    _scalar: ScalarInt,
    _ty: Ty<'tcx>,
) -> bool {
    false
}
