use crate::rustc_attr_ir::AttributeKind;
use crate::rustc_attr_ir::target::Target;
use crate::rustc_feature::AttributeStability;
use crate::rustc_lint_defs::builtin::MALFORMED_DIAGNOSTIC_ATTRIBUTES;
use crate::rustc_span::{Symbol, sym};

use crate::rustc_attr_parsing::attributes::prelude::Allow;
use crate::rustc_attr_parsing::attributes::{OnDuplicate, SingleAttributeParser};
use crate::rustc_attr_parsing::context::AcceptContext;
use crate::rustc_attr_parsing::parser::ArgParser;
use crate::rustc_attr_parsing::target_checking::AllowedTargets;
use crate::rustc_attr_parsing::{AttributeTemplate, template};

pub(crate) struct DoNotRecommendParser;
impl SingleAttributeParser for DoNotRecommendParser {
    const PATH: &[Symbol] = &[sym::diagnostic, sym::do_not_recommend];
    const ON_DUPLICATE: OnDuplicate = OnDuplicate::Warn;
    // "Allowed" on any target, noop on all but trait impls
    const ALLOWED_TARGETS: AllowedTargets<'_> =
        AllowedTargets::AllowListWarnRest(&[Allow(Target::Impl { of_trait: true })]);
    const TEMPLATE: AttributeTemplate = template!(Word /*doesn't matter */);
    const STABILITY: AttributeStability = AttributeStability::Stable;

    fn convert(cx: &mut AcceptContext<'_, '_>, args: &ArgParser) -> Option<AttributeKind> {
        let attr_span = cx.attr_span;
        if !matches!(args, ArgParser::NoArgs) {
            cx.emit_lint(
                MALFORMED_DIAGNOSTIC_ATTRIBUTES,
                crate::rustc_attr_parsing::diagnostics::DoNotRecommendDoesNotExpectArgs,
                attr_span,
            );
        }

        Some(AttributeKind::DoNotRecommend)
    }
}
