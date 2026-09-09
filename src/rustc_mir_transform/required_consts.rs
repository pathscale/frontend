// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_middle::mir::visit::Visitor;
use crate::rustc_middle::mir::{Body, ConstOperand, Location, traversal};

pub(super) struct RequiredConstsVisitor<'tcx> {
    required_consts: Vec<ConstOperand<'tcx>>,
}

impl<'tcx> RequiredConstsVisitor<'tcx> {
    pub(super) fn compute_required_consts(body: &mut Body<'tcx>) {
        let mut visitor = RequiredConstsVisitor { required_consts: Vec::new() };
        for (bb, bb_data) in traversal::reverse_postorder(&body) {
            visitor.visit_basic_block_data(bb, bb_data);
        }
        body.set_required_consts(visitor.required_consts);
    }
}

impl<'tcx> Visitor<'tcx> for RequiredConstsVisitor<'tcx> {
    fn visit_const_operand(&mut self, constant: &ConstOperand<'tcx>, _: Location) {
        if constant.const_.is_required_const() {
            self.required_consts.push(*constant);
        }
    }
}
