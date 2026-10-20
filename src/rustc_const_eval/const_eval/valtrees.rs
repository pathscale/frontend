use crate::rustc_middle::mir::interpret::{
    ErrorHandled, EvalToConstValueResult, EvalToValTreeResult, GlobalId, Scalar,
};
use crate::rustc_middle::ty::{self, TyCtxt, TypeVisitableExt};
use crate::rustc_span::DUMMY_SP;

pub(crate) fn eval_to_valtree<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    cid: GlobalId<'tcx>,
) -> EvalToValTreeResult<'tcx> {
    let unknown = || ErrorHandled::TooGeneric(tcx.def_span(cid.instance.def_id())).into();
    if cid.promoted.is_some() || cid.instance.args.has_non_region_param() {
        return Err(unknown());
    }
    // Pattern bounds and array lengths need the type-level view of the same scalar
    // constants that MIR queries fold. Aggregate and pointer values need a structural
    // conversion; do not turn their storage representation into a scalar valtree.
    let mut ty = cid.instance.ty(tcx, typing_env);
    while let ty::Pat(base, _) = *ty.kind() {
        ty = base;
    }
    if !matches!(ty.kind(), ty::Bool | ty::Int(_) | ty::Uint(_) | ty::Float(_) | ty::Char) {
        return Err(unknown());
    }
    match super::static_fold::fold_scalar(tcx, typing_env, cid) {
        Ok(Scalar::Int(value)) => Ok(ty::ValTree::from_scalar_int(tcx, value)),
        _ => Err(unknown()),
    }
}

/// Converts a structural value using the same layout-aware writer as static initializers.
pub fn valtree_to_const_value<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    cv: ty::Value<'tcx>,
) -> EvalToConstValueResult<'tcx> {
    super::static_fold::fold_valtree(tcx, typing_env, cv)
        .map_err(|_| ErrorHandled::TooGeneric(DUMMY_SP))
}
