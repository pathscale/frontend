//! Diagnostics for frontend target and codegen-attribute semantics.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_errors::{
    Diag, DiagCtxtHandle, DiagSymbolList, Diagnostic, EmissionGuarantee, Level, msg,
};
use rustc_macros::{Diagnostic, Subdiagnostic};
use crate::rustc_span::Span;

#[derive(Diagnostic)]
#[diag("`#[target_feature(..)]` cannot be applied to safe trait method")]
pub(crate) struct TargetFeatureSafeTrait {
    #[primary_span]
    #[label("cannot be applied to safe trait method")]
    pub span: Span,
    #[label("not an `unsafe` function")]
    pub def: Span,
}

#[derive(Diagnostic)]
#[diag("target feature `{$feature}` cannot be enabled with `#[target_feature]`: {$reason}")]
pub(crate) struct InternalOnlyTargetFeatureAttr<'a> {
    #[primary_span]
    pub span: Span,
    pub feature: &'a str,
    pub reason: &'a str,
}

#[derive(Diagnostic)]
#[diag("enabling the `neon` target feature on the current target is unsound due to ABI issues")]
pub(crate) struct Aarch64SoftfloatNeon;

#[derive(Diagnostic)]
#[diag(
    "enabling the `sse` target feature on the current target is unsupported due to backend issues"
)]
pub(crate) struct X86SoftfloatSse;

#[derive(Diagnostic)]
#[diag("ignoring feature with missing prefix in `-Ctarget-feature`: `{$feature}`")]
#[note("features must begin with a `+` to enable or `-` to disable it")]
pub(crate) struct UnknownCTargetFeaturePrefix<'a> {
    pub feature: &'a str,
}

#[derive(Subdiagnostic)]
pub(crate) enum PossibleFeature<'a> {
    #[help("you might have meant: `{$rust_feature}`")]
    Some { rust_feature: &'a str },
    #[help("consider filing a feature request")]
    None,
}

#[derive(Diagnostic)]
#[diag("unknown and unstable feature specified for `-Ctarget-feature`: `{$feature}`")]
#[note(
    "it is still passed through to the backend, but use of this feature might be unsound and its behavior can change"
)]
pub(crate) struct UnknownCTargetFeature<'a> {
    pub feature: &'a str,
    #[subdiagnostic]
    pub rust_feature: PossibleFeature<'a>,
}

#[derive(Diagnostic)]
#[diag("unstable feature specified for `-Ctarget-feature`: `{$feature}`")]
#[note("{$note}; its behavior can change in the future")]
pub(crate) struct UnstableCTargetFeature<'a> {
    pub feature: &'a str,
    pub note: &'a str,
}

#[derive(Diagnostic)]
#[diag("target feature `{$feature}` cannot be {$enabled} with `-Ctarget-feature`: {$reason}")]
pub(crate) struct InternalOnlyCTargetFeature<'a> {
    pub feature: &'a str,
    pub enabled: &'a str,
    pub reason: &'a str,
    #[note(
        "this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!"
    )]
    #[note(
        "for more information, see issue #116344 <https://github.com/rust-lang/rust/issues/116344>"
    )]
    pub future_compat_note: bool,
}

pub(crate) struct TargetFeatureDisableOrEnable<'a> {
    pub features: &'a [&'a str],
    pub span: Option<Span>,
    pub missing_features: Option<MissingFeatures>,
}

#[derive(Subdiagnostic)]
#[help("add the missing features in a `target_feature` attribute")]
pub(crate) struct MissingFeatures;

impl<G: EmissionGuarantee> Diagnostic<'_, G> for TargetFeatureDisableOrEnable<'_> {
    fn into_diag(self, dcx: DiagCtxtHandle<'_>, level: Level) -> Diag<'_, G> {
        let mut diag = Diag::new(
            dcx,
            level,
            msg!("the target features {$features} must all be either enabled or disabled together"),
        );
        if let Some(span) = self.span {
            diag.span(span);
        }
        if let Some(missing_features) = self.missing_features {
            diag.subdiagnostic(missing_features);
        }
        diag.arg("features", self.features.join(", "));
        diag
    }
}

#[derive(Diagnostic)]
#[diag("the feature named `{$feature}` is not valid for this target")]
pub(crate) struct FeatureNotValid<'a> {
    pub feature: &'a str,
    #[primary_span]
    #[label("`{$feature}` is not valid for this target")]
    pub span: Span,
    #[subdiagnostic]
    pub hint: FeatureNotValidHint<'a>,
    #[subdiagnostic]
    pub cross_arch: Option<CrossArchFeatureNote<'a>>,
}

#[derive(Subdiagnostic)]
pub(crate) enum CrossArchFeatureNote<'a> {
    #[note(
        "`{$feature}` is present on the `{$arch}` target architecture. Did you mean to compile for that target, or use conditional compilation?"
    )]
    Single { feature: &'a str, arch: &'a str },
    #[note(
        "`{$feature}` is present on the {$arches} target architectures. Did you mean to compile for one of those targets, or use conditional compilation?"
    )]
    Multiple { feature: &'a str, arches: DiagSymbolList<&'a str> },
}

#[derive(Subdiagnostic)]
pub(crate) enum FeatureNotValidHint<'a> {
    #[suggestion(
        "consider removing the leading `+` in the feature name",
        code = "enable = \"{stripped}\"",
        applicability = "maybe-incorrect",
        style = "verbose"
    )]
    RemovePlusFromFeatureName {
        #[primary_span]
        span: Span,
        stripped: &'a str,
    },
    #[help(
        "valid names are: {$possibilities}{$and_more ->
            [0] {\"\"}
            *[other] {\" \"}and {$and_more} more
        }"
    )]
    ValidFeatureNames { possibilities: DiagSymbolList<&'a str>, and_more: usize },
}
