use crate::rustc_middle::mir::Body;
use crate::rustc_middle::ty::TyCtxt;
use crate::rustc_mir_transform::PassPolicy;

pub(super) struct DataflowConstProp;

impl<'tcx> crate::rustc_mir_transform::MirPass<'tcx> for DataflowConstProp {
    fn policy(&self, sess: &crate::rustc_session::Session) -> PassPolicy {
        PassPolicy::optimization(sess.mir_opt_level() >= 3)
    }

    fn run_pass(&self, _tcx: TyCtxt<'tcx>, _body: &mut Body<'tcx>) {
        return;
    }
}
