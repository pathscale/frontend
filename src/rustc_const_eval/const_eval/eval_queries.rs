use alloc::format;

use crate::rustc_hir::def_id::LocalDefId;
use crate::rustc_middle::mir::ConstValue;
use crate::rustc_middle::mir::interpret::{
    ErrorHandled, EvalStaticInitializerRawResult, EvalToAllocationRawResult,
    EvalToConstValueResult, GlobalId, ReportedErrorInfo,
};
use crate::rustc_middle::ty::{self, TyCtxt, TypeVisitableExt};
use tracing::instrument;

#[instrument(skip(tcx), level = "debug")]
pub fn eval_to_const_value_raw_provider<'tcx>(
    tcx: TyCtxt<'tcx>,
    key: ty::PseudoCanonicalInput<'tcx, GlobalId<'tcx>>,
) -> EvalToConstValueResult<'tcx> {
    let def_id = key.value.instance.def_id();
    if let Some((value, _ty)) = tcx.trivial_const(def_id) {
        return Ok(value);
    }

    if key.value.instance.args.has_non_region_param() {
        return Err(ErrorHandled::TooGeneric(tcx.def_span(def_id)));
    }

    match super::static_fold::fold_const(tcx, key.typing_env, key.value) {
        Ok(value) => Ok(value),
        Err(reason) => {
            let span = tcx.def_span(def_id);
            let error = tcx.dcx().span_err(
                span,
                format!("cannot statically fold constant `{}`: {reason}", key.value.display(tcx)),
            );
            Err(ErrorHandled::Reported(ReportedErrorInfo::const_eval_error(error), span))
        }
    }
}

#[instrument(skip(tcx), level = "debug")]
pub fn eval_static_initializer_provider<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
) -> EvalStaticInitializerRawResult<'tcx> {
    assert!(tcx.is_static(def_id.to_def_id()));

    super::static_fold::fold_static_initializer(tcx, def_id).map_err(|reason| {
        let span = tcx.def_span(def_id);
        let error = tcx.dcx().span_err(
            span,
            format!("cannot statically fold initializer `{}`: {reason}", tcx.def_path_str(def_id)),
        );
        ErrorHandled::Reported(ReportedErrorInfo::const_eval_error(error), span)
    })
}

#[instrument(skip(tcx), level = "debug")]
pub fn eval_to_allocation_raw_provider<'tcx>(
    tcx: TyCtxt<'tcx>,
    key: ty::PseudoCanonicalInput<'tcx, GlobalId<'tcx>>,
) -> EvalToAllocationRawResult<'tcx> {
    Err(ErrorHandled::TooGeneric(tcx.def_span(key.value.instance.def_id())))
}
