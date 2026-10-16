use crate::rustc_middle::mir;
use crate::rustc_middle::mir::interpret::{
    ErrorHandled, EvalToConstValueResult, EvalToValTreeResult, GlobalId, Scalar,
};
use crate::rustc_middle::ty::layout::LayoutCx;
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

/// Converts values that fit a scalar or `ZeroSized` directly.
/// Values that need a backing allocation return `TooGeneric`.
pub fn valtree_to_const_value<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    cv: ty::Value<'tcx>,
) -> EvalToConstValueResult<'tcx> {
    let unknown = || ErrorHandled::TooGeneric(DUMMY_SP);

    match *cv.ty.kind() {
        ty::FnDef(..) if cv.valtree.is_zst() => Ok(mir::ConstValue::ZeroSized),
        ty::Bool | ty::Int(_) | ty::Uint(_) | ty::Float(_) | ty::Char | ty::RawPtr(_, _) => {
            Ok(mir::ConstValue::Scalar(Scalar::Int(cv.to_leaf())))
        }
        ty::Pat(base_ty, _) => {
            let cv = ty::Value { valtree: cv.valtree, ty: base_ty };
            valtree_to_const_value(tcx, typing_env, cv)
        }
        ty::Tuple(_) | ty::Array(_, _) | ty::Adt(..) => {
            let Ok(layout) = tcx.layout_of(typing_env.as_query_input(cv.ty)) else {
                return Err(unknown());
            };
            if layout.is_zst() {
                return Ok(mir::ConstValue::ZeroSized);
            }

            if layout.backend_repr.is_scalar()
                && (matches!(cv.ty.kind(), ty::Tuple(_))
                    || matches!(cv.ty.kind(), ty::Adt(def, _) if def.is_struct()))
            {
                for (i, &inner_valtree) in cv.to_branch().iter().enumerate() {
                    let field = layout.field(&LayoutCx::new(tcx, typing_env), i);
                    if !field.is_zst() {
                        let inner =
                            ty::Value { valtree: inner_valtree.to_value().valtree, ty: field.ty };
                        return valtree_to_const_value(tcx, typing_env, inner);
                    }
                }
            }

            Err(unknown())
        }
        _ => Err(unknown()),
    }
}
