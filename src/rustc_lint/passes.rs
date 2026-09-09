// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_lint_defs::LintPass;

use crate::rustc_lint::context::{EarlyContext, LateContext};

#[macro_export]
macro_rules! late_lint_methods {
    ($macro:path, $args:tt) => (
        // `_post` methods are called *after* recursing into the node.
        $macro!($args, [
            fn check_body(a: &crate::rustc_hir::Body<'tcx>);
            fn check_body_post(a: &crate::rustc_hir::Body<'tcx>);
            fn check_crate();
            fn check_crate_post();
            fn check_mod(a: &'tcx crate::rustc_hir::Mod<'tcx>, b: crate::rustc_hir::HirId);
            fn check_foreign_item(a: &'tcx crate::rustc_hir::ForeignItem<'tcx>);
            fn check_item(a: &'tcx crate::rustc_hir::Item<'tcx>);
            fn check_item_post(a: &'tcx crate::rustc_hir::Item<'tcx>);
            fn check_local(a: &'tcx crate::rustc_hir::LetStmt<'tcx>);
            fn check_block(a: &'tcx crate::rustc_hir::Block<'tcx>);
            fn check_block_post(a: &'tcx crate::rustc_hir::Block<'tcx>);
            fn check_stmt(a: &'tcx crate::rustc_hir::Stmt<'tcx>);
            fn check_arm(a: &'tcx crate::rustc_hir::Arm<'tcx>);
            fn check_pat(a: &'tcx crate::rustc_hir::Pat<'tcx>);
            fn check_lit(hir_id: crate::rustc_hir::HirId, a: crate::rustc_hir::Lit, is_negated_pat: bool);
            fn check_expr(a: &'tcx crate::rustc_hir::Expr<'tcx>);
            fn check_expr_post(a: &'tcx crate::rustc_hir::Expr<'tcx>);
            fn check_ty(a: &'tcx crate::rustc_hir::Ty<'tcx, crate::rustc_hir::AmbigArg>);
            fn check_generic_param(a: &'tcx crate::rustc_hir::GenericParam<'tcx>);
            fn check_generics(a: &'tcx crate::rustc_hir::Generics<'tcx>);
            fn check_poly_trait_ref(a: &'tcx crate::rustc_hir::PolyTraitRef<'tcx>);
            fn check_fn(
                a: crate::rustc_hir::intravisit::FnKind<'tcx>,
                b: &'tcx crate::rustc_hir::FnDecl<'tcx>,
                c: &'tcx crate::rustc_hir::Body<'tcx>,
                d: crate::rustc_span::Span,
                e: crate::rustc_span::def_id::LocalDefId);
            fn check_trait_item(a: &'tcx crate::rustc_hir::TraitItem<'tcx>);
            fn check_impl_item(a: &'tcx crate::rustc_hir::ImplItem<'tcx>);
            fn check_impl_item_post(a: &'tcx crate::rustc_hir::ImplItem<'tcx>);
            fn check_field_def(a: &'tcx crate::rustc_hir::FieldDef<'tcx>);
            fn check_variant(a: &'tcx crate::rustc_hir::Variant<'tcx>);
            fn check_path(a: &crate::rustc_hir::Path<'tcx>, b: crate::rustc_hir::HirId);
            fn check_attribute(a: &'tcx crate::rustc_hir::Attribute);
            fn check_attributes(a: &'tcx [crate::rustc_hir::Attribute]);
            fn check_attributes_post(a: &'tcx [crate::rustc_hir::Attribute]);
        ]);
    )
}

/// Trait for types providing lint checks.
///
/// Each `check` method checks a single syntax node, and should not
/// invoke methods recursively (unlike `Visitor`). By default they
/// do nothing.
///
// FIXME: eliminate the duplication with `Visitor`. But this also
// contains a few lint-specific methods with no equivalent in `Visitor`.
//
macro_rules! declare_late_lint_pass {
    ([], [$(fn $name:ident($($param:ident: $arg:ty),*);)*]) => (
        pub trait LateLintPass<'tcx>: LintPass {
            $(#[inline(always)] fn $name(&mut self, _: &LateContext<'tcx>, $(_: $arg),*) {})*
        }
    )
}

// Declare the `LateLintPass` trait, which contains empty default definitions
// for all the `check_*` methods.
late_lint_methods!(declare_late_lint_pass, []);

#[macro_export]
macro_rules! expand_combined_late_lint_pass_method {
    ([$($pass:ident),*], $self: ident, $name: ident, $params:tt) => ({
        $($self.$pass.$name $params;)*
    })
}

#[macro_export]
macro_rules! expand_combined_late_lint_pass_methods {
    ($passes:tt, [$(fn $name:ident($($param:ident: $arg:ty),*);)*]) => (
        $(fn $name(&mut self, context: &$crate::rustc_lint::LateContext<'tcx>, $($param: $arg),*) {
            $crate::expand_combined_late_lint_pass_method!($passes, self, $name, (context, $($param),*));
        })*
    )
}

/// Combines multiple lints passes into a single lint pass, at compile time,
/// for maximum speed. Each `check_foo` method in `$methods` within this pass
/// simply calls `check_foo` once per `$pass`. Compare with
/// `RuntimeCombinedLateLintPass`, which is similar, but combines lint passes at
/// runtime.
#[macro_export]
macro_rules! declare_combined_late_lint_pass {
    ([$v:vis $name:ident, [$($pass:ident: $constructor:expr,)*]], $methods:tt) => (
        #[allow(non_snake_case)]
        $v struct $name {
            $($pass: $pass,)*
        }

        impl $name {
            $v fn new() -> Self {
                Self {
                    $($pass: $constructor,)*
                }
            }

            $v fn lint_vec() -> $crate::rustc_lint::LintVec {
                let mut lints = Vec::new();
                $(lints.extend_from_slice(&$pass::lint_vec());)*
                lints
            }
        }

        impl<'tcx> $crate::rustc_lint::LateLintPass<'tcx> for $name {
            $crate::expand_combined_late_lint_pass_methods!([$($pass),*], $methods);
        }

        #[allow(rustc::lint_pass_impl_without_macro)]
        impl $crate::rustc_lint::LintPass for $name {
            fn name(&self) -> &'static str {
                stringify!($name)
            }
            fn get_lints(&self) -> LintVec {
                $name::lint_vec()
            }
        }
    )
}

#[macro_export]
macro_rules! early_lint_methods {
    ($macro:path, $args:tt) => (
        // `_post` methods are called *after* recursing into the node.
        $macro!($args, [
            fn check_param(a: &crate::rustc_ast::Param);
            fn check_ident(a: &crate::rustc_span::Ident);
            fn check_crate(a: &crate::rustc_ast::Crate);
            fn check_crate_post(a: &crate::rustc_ast::Crate);
            fn check_item(a: &crate::rustc_ast::Item);
            fn check_item_post(a: &crate::rustc_ast::Item);
            fn check_local(a: &crate::rustc_ast::Local);
            fn check_block(a: &crate::rustc_ast::Block);
            fn check_stmt(a: &crate::rustc_ast::Stmt);
            fn check_arm(a: &crate::rustc_ast::Arm);
            fn check_pat(a: &crate::rustc_ast::Pat);
            fn check_pat_post(a: &crate::rustc_ast::Pat);
            fn check_expr(a: &crate::rustc_ast::Expr);
            fn check_expr_post(a: &crate::rustc_ast::Expr);
            fn check_ty(a: &crate::rustc_ast::Ty);
            fn check_generic_arg(a: &crate::rustc_ast::GenericArg);
            fn check_generic_param(a: &crate::rustc_ast::GenericParam);
            fn check_generics(a: &crate::rustc_ast::Generics);
            fn check_poly_trait_ref(a: &crate::rustc_ast::PolyTraitRef);
            fn check_fn(
                a: crate::rustc_ast::visit::FnKind<'_>,
                c: crate::rustc_span::Span,
                d_: crate::rustc_ast::NodeId);
            fn check_trait_item(a: &crate::rustc_ast::AssocItem);
            fn check_trait_item_post(a: &crate::rustc_ast::AssocItem);
            fn check_impl_item(a: &crate::rustc_ast::AssocItem);
            fn check_impl_item_post(a: &crate::rustc_ast::AssocItem);
            fn check_variant(a: &crate::rustc_ast::Variant);
            fn check_attribute(a: &crate::rustc_ast::Attribute);
            fn check_attributes(a: &[crate::rustc_ast::Attribute]);
            fn check_attributes_post(a: &[crate::rustc_ast::Attribute]);
            fn check_mac_def(a: &crate::rustc_ast::MacroDef);
            fn check_mac(a: &crate::rustc_ast::MacCall);
            fn check_where_predicate(a: &crate::rustc_ast::WherePredicate);
            fn check_where_predicate_post(a: &crate::rustc_ast::WherePredicate);
        ]);
    )
}

macro_rules! declare_early_lint_pass {
    ([], [$(fn $name:ident($($param:ident: $arg:ty),*);)*]) => (
        pub trait EarlyLintPass: LintPass {
            $(#[inline(always)] fn $name(&mut self, _: &EarlyContext<'_>, $(_: $arg),*) {})*
        }
    )
}

// Declare the `EarlyLintPass` trait, which contains empty default definitions
// for all the `check_*` methods.
early_lint_methods!(declare_early_lint_pass, []);

#[macro_export]
macro_rules! expand_combined_early_lint_pass_method {
    ([$($pass:ident),*], $self: ident, $name: ident, $params:tt) => ({
        $($self.$pass.$name $params;)*
    })
}

#[macro_export]
macro_rules! expand_combined_early_lint_pass_methods {
    ($passes:tt, [$(fn $name:ident($($param:ident: $arg:ty),*);)*]) => (
        $(fn $name(&mut self, context: &$crate::rustc_lint::EarlyContext<'_>, $($param: $arg),*) {
            $crate::expand_combined_early_lint_pass_method!($passes, self, $name, (context, $($param),*));
        })*
    )
}

/// Combines multiple lints passes into a single lint pass, at compile time,
/// for maximum speed. Each `check_foo` method in `$methods` within this pass
/// simply calls `check_foo` once per `$pass`. Compare with
/// `RuntimeCombinedEarlyLintPass`, which is similar, but combines lint passes at
/// runtime.
#[macro_export]
macro_rules! declare_combined_early_lint_pass {
    ([$v:vis $name:ident, [$($pass:ident: $constructor:expr,)*]], $methods:tt) => (
        #[allow(non_snake_case)]
        $v struct $name {
            $($pass: $pass,)*
        }

        impl $name {
            $v fn new() -> Self {
                Self {
                    $($pass: $constructor,)*
                }
            }

            $v fn lint_vec() -> $crate::rustc_lint::LintVec {
                let mut lints = Vec::new();
                $(lints.extend_from_slice(&$pass::lint_vec());)*
                lints
            }
        }

        impl $crate::rustc_lint::EarlyLintPass for $name {
            $crate::expand_combined_early_lint_pass_methods!([$($pass),*], $methods);
        }

        #[allow(rustc::lint_pass_impl_without_macro)]
        impl $crate::rustc_lint::LintPass for $name {
            fn name(&self) -> &'static str {
                panic!()
            }
            fn get_lints(&self) -> LintVec {
                panic!()
            }
        }
    )
}

/// A lint pass boxed up as a trait object.
pub(crate) type EarlyLintPassObject = Box<dyn EarlyLintPass>;
pub(crate) type LateLintPassObject<'tcx> = Box<dyn LateLintPass<'tcx> + 'tcx>;
