//! Declares an arena that can allocate values of any `Copy` type, and of
//! any `!Copy` type listed below.

crate::rustc_arena::declare_arena! {
    // HIR types
    asm_template: crate::rustc_ast::InlineAsmTemplatePiece,
    attribute: crate::rustc_attr_ir::Attribute,
    owner_info: crate::rustc_hir::OwnerInfo<'tcx>,
    macro_def: crate::rustc_ast::MacroDef,
    delegation_info: crate::rustc_hir::DelegationInfo,
}
