// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_hir::{Expr, ExprKind};
use crate::rustc_span::sym;

use crate::rustc_lint::LateContext;

/// Given an expression, peel all of casts (`<expr> as ...`, `<expr>.cast{,_mut,_const}()`,
/// `ptr::from_ref(<expr>)`, ...) and init expressions.
///
/// Returns the innermost expression.
pub(crate) fn peel_casts<'tcx>(
    cx: &LateContext<'tcx>,
    mut e: &'tcx Expr<'tcx>,
) -> &'tcx Expr<'tcx> {
    loop {
        e = e.peel_blocks();
        // <expr> as ...
        e = if let ExprKind::Cast(expr, _) = e.kind {
            expr
        // <expr>.cast(), <expr>.cast_mut() or <expr>.cast_const()
        } else if let ExprKind::MethodCall(_, expr, [], _) = e.kind
            && let Some(def_id) = cx.typeck_results().type_dependent_def_id(e.hir_id)
            && matches!(
                cx.tcx.get_diagnostic_name(def_id),
                Some(sym::ptr_cast | sym::const_ptr_cast | sym::ptr_cast_mut | sym::ptr_cast_const)
            )
        {
            expr
        // ptr::from_ref(<expr>), UnsafeCell::raw_get(<expr>) or mem::transmute<_, _>(<expr>)
        } else if let ExprKind::Call(path, [arg]) = e.kind
            && let ExprKind::Path(ref qpath) = path.kind
            && let Some(def_id) = cx.qpath_res(qpath, path.hir_id).opt_def_id()
            && matches!(
                cx.tcx.get_diagnostic_name(def_id),
                Some(sym::ptr_from_ref | sym::unsafe_cell_raw_get | sym::transmute)
            )
        {
            arg
        } else {
            let init = cx.expr_or_init(e);
            if init.hir_id != e.hir_id {
                init
            } else {
                break;
            }
        };
    }

    e
}
