use crate::rustc_attr_ir::AttributeKind;
use crate::rustc_attr_ir::target::Target;
use crate::rustc_feature::AttributeStability;
use crate::rustc_lint_defs::builtin::MALFORMED_DIAGNOSTIC_ATTRIBUTES;
use crate::rustc_span::{Span, sym};

use crate::rustc_attr_parsing::attributes::diagnostic::gate_diagnostic_attr;
use crate::rustc_attr_parsing::attributes::{AcceptMapping, AttributeParser};
use crate::rustc_attr_parsing::context::{AcceptContext, FinalizeContext};
use crate::rustc_attr_parsing::diagnostics::OpaqueDoesNotExpectArgs;
use crate::rustc_attr_parsing::parser::ArgParser;
use crate::rustc_attr_parsing::target_checking::AllowedTargets;
use crate::rustc_attr_parsing::target_checking::Policy::Allow;
use crate::rustc_attr_parsing::{template, unstable};

#[derive(Default)]
pub(crate) struct OpaqueParser {
    attr_span: Option<Span>,
}

impl AttributeParser for OpaqueParser {
    const ATTRIBUTES: AcceptMapping<Self> = &[
        (
            &[sym::diagnostic, sym::opaque],
            template!(Word),
            AttributeStability::Stable, // Unstable, stability checked manually below
            |this, cx, args| {
                gate_diagnostic_attr!(cx, args, diagnostic_opaque);
                this.parse(cx, args);
            },
        ),
        (
            // For use on exported macros, where using tool attributes is an error.
            &[sym::rustc_diagnostic_opaque],
            template!(Word),
            unstable!(
                rustc_attrs,
                "see `#[diagnostic::opaque]` for the nightly equivalent of this attribute"
            ),
            OpaqueParser::parse,
        ),
    ];
    const ALLOWED_TARGETS: AllowedTargets<'_> =
        AllowedTargets::AllowListWarnRest(&[Allow(Target::MacroDef)]);

    fn finalize(self, _cx: &FinalizeContext<'_, '_>) -> Option<AttributeKind> {
        if self.attr_span.is_some() { Some(AttributeKind::Opaque) } else { None }
    }
}

impl OpaqueParser {
    fn parse<'sess>(&mut self, cx: &mut AcceptContext<'_, 'sess>, args: &ArgParser) {
        let attr_span = cx.attr_span;
        if let Some(earlier_span) = self.attr_span {
            cx.warn_unused_duplicate(earlier_span, attr_span);
        }
        self.attr_span = Some(attr_span);

        if !matches!(args, ArgParser::NoArgs) {
            cx.emit_lint(MALFORMED_DIAGNOSTIC_ATTRIBUTES, OpaqueDoesNotExpectArgs, attr_span);
        }
    }
}
