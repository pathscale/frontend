// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_span::Span;
use crate::rustc_type_ir::elaborate::Elaboratable;

use crate::rustc_middle::ty::{self, TyCtxt};

impl<'tcx> Elaboratable<TyCtxt<'tcx>> for ty::Clause<'tcx> {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        self.as_predicate()
    }

    fn child(&self, clause: ty::Clause<'tcx>) -> Self {
        clause
    }

    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitClause<'tcx>,
        _index: usize,
    ) -> Self {
        clause
    }
}

impl<'tcx> Elaboratable<TyCtxt<'tcx>> for ty::Predicate<'tcx> {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        *self
    }

    fn child(&self, clause: ty::Clause<'tcx>) -> Self {
        clause.as_predicate()
    }

    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitClause<'tcx>,
        _index: usize,
    ) -> Self {
        clause.as_predicate()
    }
}

impl<'tcx> Elaboratable<TyCtxt<'tcx>> for (ty::Predicate<'tcx>, Span) {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        self.0
    }

    fn child(&self, clause: ty::Clause<'tcx>) -> Self {
        (clause.as_predicate(), self.1)
    }

    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitClause<'tcx>,
        _index: usize,
    ) -> Self {
        (clause.as_predicate(), self.1)
    }
}

impl<'tcx> Elaboratable<TyCtxt<'tcx>> for (ty::Clause<'tcx>, Span) {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        self.0.as_predicate()
    }

    fn child(&self, clause: ty::Clause<'tcx>) -> Self {
        (clause, self.1)
    }

    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitClause<'tcx>,
        _index: usize,
    ) -> Self {
        (clause, self.1)
    }
}
