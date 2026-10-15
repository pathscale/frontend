use crate::rustc_middle::mir::Body;
use crate::rustc_middle::ty::TyCtxt;
use crate::rustc_mir_transform::PassPolicy;

pub(super) struct Gvn;

impl<'tcx> crate::rustc_mir_transform::MirPass<'tcx> for Gvn {
    fn policy(&self, sess: &crate::rustc_session::Session) -> PassPolicy {
        PassPolicy::optimization(sess.mir_opt_level() >= 2)
    }

    fn run_pass(&self, _tcx: TyCtxt<'tcx>, _body: &mut Body<'tcx>) {
    }
}
