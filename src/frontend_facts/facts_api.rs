//! A narrow, stable surface of parser and crate facts for code that builds on this crate.

pub use super::site::{
    Binding, BindingKind, FnSignature as SiteFnSignature, ItemCategory, Position, Site, TextRange,
    site_at,
};
pub use super::syntax::declares_no_core_source;
pub use super::{CrateFacts, Definition, FactKind, Loaded, ModuleNames};
