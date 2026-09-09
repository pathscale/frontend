use crate::rustc_attr_ir::AttributeKind;
use crate::rustc_attr_ir::target::Target;
use crate::rustc_feature::AttributeStability;
use crate::rustc_span::{Span, Symbol, sym};

use crate::rustc_attr_parsing::attributes::{NoArgsAttributeParser, OnDuplicate};
use crate::rustc_attr_parsing::target_checking::AllowedTargets;
use crate::rustc_attr_parsing::target_checking::Policy::{Allow, Warn};

pub(crate) struct NonExhaustiveParser;

impl NoArgsAttributeParser for NonExhaustiveParser {
    const PATH: &[Symbol] = &[sym::non_exhaustive];
    const ON_DUPLICATE: OnDuplicate = OnDuplicate::Warn;
    const ALLOWED_TARGETS: AllowedTargets<'_> = AllowedTargets::AllowList(&[
        Allow(Target::Enum),
        Allow(Target::Struct),
        Allow(Target::Variant),
        Warn(Target::Field),
        Warn(Target::Arm),
        Warn(Target::MacroDef),
        Warn(Target::MacroCall),
    ]);
    const STABILITY: AttributeStability = AttributeStability::Stable;
    const CREATE: fn(Span) -> AttributeKind = AttributeKind::NonExhaustive;
}
