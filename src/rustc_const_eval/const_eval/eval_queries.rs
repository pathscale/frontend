use crate::rustc_hir::def_id::LocalDefId;
use crate::rustc_middle::mir::interpret::{
    ErrorHandled, EvalStaticInitializerRawResult, EvalToAllocationRawResult,
    EvalToConstValueResult, GlobalId,
};
use crate::rustc_middle::ty::{self, TyCtxt};
use tracing::instrument;

#[instrument(skip(tcx), level = "debug")]
pub fn eval_to_const_value_raw_provider<'tcx>(
    tcx: TyCtxt<'tcx>,
    key: ty::PseudoCanonicalInput<'tcx, GlobalId<'tcx>>,
) -> EvalToConstValueResult<'tcx> {
    if let Some((value, _ty)) = tcx.trivial_const(key.value.instance.def_id()) {
        return Ok(value);
    }

    Err(ErrorHandled::TooGeneric(tcx.def_span(key.value.instance.def_id())))
}

#[instrument(skip(tcx), level = "debug")]
pub fn eval_static_initializer_provider<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
) -> EvalStaticInitializerRawResult<'tcx> {
    assert!(tcx.is_static(def_id.to_def_id()));

    Err(ErrorHandled::TooGeneric(tcx.def_span(def_id)))
}

#[instrument(skip(tcx), level = "debug")]
pub fn eval_to_allocation_raw_provider<'tcx>(
    tcx: TyCtxt<'tcx>,
    key: ty::PseudoCanonicalInput<'tcx, GlobalId<'tcx>>,
) -> EvalToAllocationRawResult<'tcx> {
    Err(ErrorHandled::TooGeneric(tcx.def_span(key.value.instance.def_id())))
}
