//! `P<T>`, the owning pointer for boxed AST nodes.
//!
//! Today it is `Box<T>`. It is the one place a later change points AST edges at an arena, so
//! signatures name `P<Expr>` rather than `Box<Expr>` and an upstream `Box<Expr>` fails here
//! instead of silently allocating.

pub type P<T> = alloc::boxed::Box<T>;
