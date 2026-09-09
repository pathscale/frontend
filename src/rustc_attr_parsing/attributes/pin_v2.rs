use crate::rustc_attr_ir::AttributeKind;
use crate::rustc_attr_ir::target::Target;
use crate::rustc_feature::AttributeStability;
use crate::rustc_span::{Span, Symbol, sym};

use crate::rustc_attr_parsing::attributes::NoArgsAttributeParser;
use crate::rustc_attr_parsing::target_checking::AllowedTargets;
use crate::rustc_attr_parsing::target_checking::Policy::Allow;
use crate::unstable;

pub(crate) struct PinV2Parser;

impl NoArgsAttributeParser for PinV2Parser {
    const PATH: &[Symbol] = &[sym::pin_v2];
    const ALLOWED_TARGETS: AllowedTargets<'_> = AllowedTargets::AllowList(&[
        Allow(Target::Enum),
        Allow(Target::Struct),
        Allow(Target::Union),
    ]);
    const STABILITY: AttributeStability = unstable!(pin_ergonomics);
    const CREATE: fn(Span) -> AttributeKind = AttributeKind::PinV2;
}
