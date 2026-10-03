use crate::rustc_abi::VariantIdx;
use crate::rustc_middle::mir;
use crate::rustc_middle::query::Providers;
use crate::rustc_middle::ty::{self, ScalarInt, Ty, TyCtxt};

mod eval_queries;
mod fn_queries;
mod valtrees;

pub use self::eval_queries::*;
pub use self::fn_queries::*;
pub(crate) use self::valtrees::{eval_to_valtree, valtree_to_const_value};

pub fn provide(providers: &mut Providers) {
    fn_queries::provide(providers);
}

pub(crate) fn try_destructure_mir_constant_for_user_output<'tcx>(
    _tcx: TyCtxt<'tcx>,
    _val: mir::ConstValue,
    _ty: Ty<'tcx>,
) -> Option<mir::DestructuredConstant<'tcx>> {
    // Aggregate field decomposition is unavailable without an interpreter context.
    None
}

pub fn tag_for_variant_provider<'tcx>(
    _tcx: TyCtxt<'tcx>,
    _key: ty::PseudoCanonicalInput<'tcx, (Ty<'tcx>, VariantIdx)>,
) -> Option<ScalarInt> {
    None
}
