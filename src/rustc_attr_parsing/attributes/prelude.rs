// data structures
#[doc(hidden)]
pub(super) use crate::rustc_attr_ir::AttributeKind;
#[doc(hidden)]
pub(super) use crate::rustc_attr_ir::target::{AssocCtxt, MethodKind, Target};
#[doc(hidden)]
pub(super) use crate::rustc_span::{Ident, Span, Symbol, sym};
#[doc(hidden)]
pub(super) use thin_vec::ThinVec;

#[doc(hidden)]
pub(super) use crate::rustc_attr_parsing::attributes::{
    AcceptMapping, AttributeParser, CombineAttributeParser, ConvertFn, NoArgsAttributeParser,
    OnDuplicate, SingleAttributeParser,
};
// contexts
#[doc(hidden)]
pub(super) use crate::rustc_attr_parsing::context::{AcceptContext, FinalizeCheckContext, FinalizeContext};
#[doc(hidden)]
pub(super) use crate::rustc_attr_parsing::parser::*;
// target checking
#[doc(hidden)]
pub(super) use crate::rustc_attr_parsing::target_checking::Policy::{Allow, Error, Warn};
#[doc(hidden)]
pub(super) use crate::rustc_attr_parsing::target_checking::{ALL_TARGETS, AllowedTargets};
#[doc(hidden)]
pub(super) use crate::unstable;
pub(super) use crate::rustc_attr_parsing::{AttributeTemplate, template};
