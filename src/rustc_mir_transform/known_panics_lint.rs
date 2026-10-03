use crate::rustc_middle::mir::Body;
use crate::rustc_middle::ty::TyCtxt;

pub(super) struct KnownPanicsLint;

impl<'tcx> crate::rustc_mir_transform::MirLint<'tcx> for KnownPanicsLint {
    fn run_lint(&self, _tcx: TyCtxt<'tcx>, _body: &Body<'tcx>) {
        return;
    }
}
