use frontend::frontend_facts::effects::{
    EnclosingBodySpanError, StaticFact, TextRange, enclosing_function_body_span,
};
use frontend::frontend_facts::{
    BodyTypeCoverageGapKind, BodyTypeDesugaringKind, BodyTypeFacts, CrateRead, Dependency,
    Loaded, ResolvedOptionVariantRole, ResolvedPrimitive, ResolvedTypeIdentity,
    ResolvedVariantIdentity, Severity, analyze_body_type_facts,
    analyze_body_type_facts_with_loaded, analyze_body_type_identity,
    analyze_body_type_identity_with_loaded, read_crate,
};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn ready() {
    frontend::unwind_janky::install_catcher(catcher);
}

fn range(source: &str, text: &str) -> TextRange {
    let start = source.find(text).expect("source range") as u32;
    TextRange { start, end: start + text.len() as u32 }
}

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("body-type-identity-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create test scratch directory");
        Self(path)
    }

    fn write(&self, name: &str, source: &str) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, source).expect("write test source");
        path.to_str().expect("test path is UTF-8").to_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_generated_range_fact(
    facts: &BodyTypeFacts,
    source: &str,
    statement: &str,
    expression: &str,
) {
    let statement_start = source.find(statement).expect("range statement") as u32;
    let expression_start =
        statement_start + statement.find(expression).expect("range expression") as u32;
    let expression_end = expression_start + expression.len() as u32;
    assert!(
        facts.generated_expressions.iter().any(|fact| {
            fact.desugaring_chain.contains(&BodyTypeDesugaringKind::RangeExpr)
                && fact.callsite.start <= expression_start
                && fact.callsite.end >= expression_end
                && matches!(&fact.ty, StaticFact::Known { .. })
                && matches!(&fact.adjusted_ty, StaticFact::Known { .. })
        }),
        "no known RangeExpr fact covers `{expression}`: {facts:?}"
    );
}

#[test]
fn parser_selects_structural_innermost_function_body_without_a_name() {
    ready();
    let source = "fn outer() { fn inner() -> i32 { 7 } }";
    let candidate = range(source, "7");
    let body = enclosing_function_body_span(source, candidate).expect("inner function body");
    assert_eq!(body, range(source, "{ 7 }"));

    let facts = analyze_body_type_facts("body_type_facts", source, body);
    assert_eq!(
        facts.body_span.as_ref().map(|span| (span.start, span.end)),
        Some((body.start, body.end))
    );
}

#[test]
fn parser_selector_refuses_invalid_ranges_and_reports_parse_errors() {
    ready();
    let source = "fn f() { 'é'; }";
    assert_eq!(
        enclosing_function_body_span(source, TextRange { start: 11, end: 12 }),
        Err(EnclosingBodySpanError::InvalidRange)
    );
    assert!(matches!(
        enclosing_function_body_span("fn f( {", TextRange { start: 5, end: 6 }),
        Err(EnclosingBodySpanError::ParseFailed { diagnostics }) if !diagnostics.is_empty()
    ));
}

#[test]
fn primitive_expression_has_a_computed_type_and_full_body_span() {
    ready();
    let source = "fn primitive() -> i32 { let value = 7; value }";
    let body = range(source, "{ let value = 7; value }");
    let facts = analyze_body_type_facts("body_type_facts", source, body);

    assert!(!facts.fatal, "{facts:?}");
    assert_eq!(facts.typeck_tainted_by_errors, Some(false));
    assert!(facts.coverage_gaps.is_empty(), "{facts:?}");
    assert!(facts.type_extraction_complete, "{facts:?}");
    assert!(facts.projection_gaps.is_empty(), "{facts:?}");
    assert!(facts.uncomputed_consts.is_empty(), "{facts:?}");
    assert_eq!(
        facts.body_span.as_ref().map(|span| (span.start, span.end)),
        Some((body.start, body.end))
    );
    let literal = source.find("7;").expect("literal") as u32;
    let fact = facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == literal)
        .expect("typed literal expression");
    assert_eq!(fact.ty, StaticFact::Known { value: "i32".to_string() });
}

#[test]
fn resolved_identities_use_rustc_types_and_refuse_the_injected_option_stub() {
    ready();
    let shadowed = "enum Option<T> { Some(T), None } struct Wrapper<T>(T); enum Shape { Record { value: u8 } } fn body() { let selected = Option::Some(1u8); let absent: Option<u8> = Option::None; let wrapped8 = Wrapper(1u8); let wrapped16 = Wrapper(1u16); let record = Shape::Record { value: 1u8 }; selected; absent; wrapped8; wrapped16; record; }";
    let body = range(shadowed, "{ let selected = Option::Some(1u8); let absent: Option<u8> = Option::None; let wrapped8 = Wrapper(1u8); let wrapped16 = Wrapper(1u16); let record = Shape::Record { value: 1u8 }; selected; absent; wrapped8; wrapped16; record; }");
    let report = analyze_body_type_identity("body_identity_shadowed_option", shadowed, body);
    assert!(!report.type_facts.fatal, "{:?}", report.type_facts);
    assert!(!report.identity_facts.extraction_complete, "unsupported function-item identities stay explicit: {:?}", report.identity_facts);
    assert!(report.identity_facts.coverage_gaps.iter().any(|gap| gap.kind == BodyTypeCoverageGapKind::ExpressionTypeMissing), "{:?}", report.identity_facts);
    let identity_at = |text: &str| {
        let start = shadowed.find(text).expect("source marker") as u32;
        let end = start + text.len() as u32;
        report
            .identity_facts
            .expressions
            .iter()
            .find(|fact| fact.span.start == start && fact.span.end == end)
            .expect("source-mapped identity fact")
    };
    assert!(matches!(
        &identity_at("Option::Some(1u8)").resolved_ty,
        StaticFact::Known { value: ResolvedTypeIdentity::Adt { arguments, .. } }
            if arguments == &[ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::U8 }]
    ));
    assert!(matches!(
        &identity_at("1u8").resolved_ty,
        StaticFact::Known { value: ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::U8 } }
    ));
    assert!(matches!(
        &identity_at("Option::Some(1u8)").resolved_variant,
        StaticFact::Known { value: Some(_) }
    ));
    assert!(matches!(
        &identity_at("Option::None").resolved_variant,
        StaticFact::Known { value: Some(_) }
    ));
    assert!(matches!(
        &identity_at("Option::Some(1u8)").resolved_variant,
        StaticFact::Known { value: Some(ResolvedVariantIdentity { option_role: None, .. }) }
    ));
    assert_ne!(
        &identity_at("Wrapper(1u8)").resolved_ty,
        &identity_at("Wrapper(1u16)").resolved_ty,
        "generic arguments distinguish otherwise identical ADT definitions"
    );
    assert!(matches!(
        &identity_at("Shape::Record { value: 1u8 }").resolved_variant,
        StaticFact::Known { value: Some(_) }
    ));

    let injected = "fn body() -> Option<u8> { Some(1u8) }";
    let injected_body = range(injected, "{ Some(1u8) }");
    let injected_report = analyze_body_type_identity("body_identity_injected_option", injected, injected_body);
    let start = injected.find("Some(1u8)").expect("synthetic Option use") as u32;
    let end = start + "Some(1u8)".len() as u32;
    let synthetic_fact = injected_report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == start && fact.span.end == end)
        .expect("source-mapped synthetic Option expression");
    assert!(matches!(
        &synthetic_fact.resolved_ty,
        StaticFact::Unknown { why } if why == "synthetic_lang_item_context"
    ));
    assert!(matches!(
        &synthetic_fact.resolved_variant,
        StaticFact::Unknown { why } if why == "synthetic_lang_item_context"
    ));
    assert!(!injected_report.identity_facts.extraction_complete);
    assert!(injected_report.identity_facts.coverage_gaps.iter().any(|gap| {
        gap.kind == BodyTypeCoverageGapKind::ExpressionTypeMissing
    }));
}

#[test]
fn resolved_option_lang_item_and_variant_follow_external_metadata() {
    ready();
    let scratch = Scratch::new("loaded-option");
    let option_source = "#![feature(no_core, lang_items)]\n#![allow(internal_features)]\n#![no_core]\n#[lang = \"pointee_sized\"] pub trait PointeeSized {}\n#[lang = \"meta_sized\"] pub trait MetaSized: PointeeSized {}\n#[lang = \"sized\"] pub trait Sized: MetaSized {}\n#[lang = \"copy\"] pub trait Copy {}\n#[lang = \"legacy_receiver\"] pub trait LegacyReceiver {}\nimpl<T: ?Sized> LegacyReceiver for &T {}\nimpl Copy for u32 {}\n#[lang = \"Option\"] pub enum Option<T> { #[lang = \"None\"] None, #[lang = \"Some\"] Some(T) }\n";
    let root = scratch.write("optionlang.rs", option_source);
    let metadata = scratch.0.join("liboptionlang.rmeta");
    let metadata_text = metadata.to_str().expect("metadata path is UTF-8").to_owned();
    read_crate(&CrateRead {
        edition: Some("2024"),
        items_only: true,
        write_metadata: Some(eko::path::Path::new(&metadata_text)),
        ..CrateRead::new("optionlang", eko::path::Path::new(&root))
    })
    .expect("write the fixture's Option lang-item metadata");
    let dependencies = [Dependency::new("optionlang", metadata_text)];
    let source = "#![feature(no_core)]\n#![no_core]\nextern crate optionlang;\nfn body() -> optionlang::Option<u32> { let present = optionlang::Option::Some(1u32); let absent: optionlang::Option<u32> = optionlang::Option::None; present }";
    let body = range(source, "{ let present = optionlang::Option::Some(1u32); let absent: optionlang::Option<u32> = optionlang::Option::None; present }");
    let report = analyze_body_type_identity_with_loaded(
        "body_identity_loaded_option",
        source,
        body,
        Some("2024"),
        Loaded { dependencies: &dependencies, ..Loaded::default() },
    );
    let marker = "optionlang::Option::Some(1u32)";
    let start = source.find(marker).expect("variant expression") as u32;
    let end = start + marker.len() as u32;
    let expression = report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == start && fact.span.end == end)
        .expect("source-mapped Option expression");
    let StaticFact::Known { value: ResolvedTypeIdentity::OptionLangItem { definition, argument } } = &expression.resolved_ty else {
        panic!("external Option lang item was not resolved: {expression:?}");
    };
    assert!(matches!(argument.as_ref(), ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::U32 }));
    let StaticFact::Known { value: Some(variant) } = &expression.resolved_variant else {
        panic!("external Option variant was not resolved: {expression:?}");
    };
    assert_eq!(&variant.parent_enum, definition);
    assert_eq!(variant.option_role, Some(ResolvedOptionVariantRole::Some));

    let marker = "optionlang::Option::None";
    let start = source.find(marker).expect("None variant expression") as u32;
    let end = start + marker.len() as u32;
    let none_expression = report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == start && fact.span.end == end)
        .expect("source-mapped None expression");
    let StaticFact::Known { value: Some(variant) } = &none_expression.resolved_variant else {
        panic!("external Option None variant was not resolved: {none_expression:?}");
    };
    assert_eq!(variant.option_role, Some(ResolvedOptionVariantRole::None));
}

#[test]
fn ill_typed_body_keeps_diagnostics_and_marks_unavailable_expression_types() {
    ready();
    let source = "fn broken() -> bool { true + 1 }";
    let body = range(source, "{ true + 1 }");
    let facts = analyze_body_type_facts("body_type_facts_ill_typed", source, body);

    assert!(facts.body_span.is_some());
    assert_eq!(facts.typeck_tainted_by_errors, Some(true));
    assert!(!facts.type_extraction_complete, "{facts:?}");
    assert!(facts.diagnostics.iter().any(|diag| diag.severity == Severity::Error));
    assert!(
        facts
            .coverage_gaps
            .iter()
            .any(|gap| { gap.kind == BodyTypeCoverageGapKind::ExpressionTypeMissing })
    );
    assert!(
        facts
            .expressions
            .iter()
            .any(|fact| { fact.ty == StaticFact::Known { value: "bool".to_string() } })
    );
}

#[test]
fn invalid_edition_is_returned_as_a_fatal_coverage_gap() {
    ready();
    let source = "fn configured() -> i32 { 1 }";
    let body = range(source, "{ 1 }");
    let facts = analyze_body_type_facts_with_loaded(
        "body_type_facts_invalid_edition",
        source,
        body,
        Some("not-an-edition"),
        Loaded::default(),
    );
    assert!(facts.fatal);
    assert!(
        facts
            .coverage_gaps
            .iter()
            .any(|gap| { gap.kind == BodyTypeCoverageGapKind::FrontendFatal })
    );
}

#[test]
fn an_inner_candidate_range_does_not_select_its_function_body() {
    ready();
    let source = "fn exact() -> i32 { 7 }";
    let inner = range(source, "7");
    assert_eq!(
        analyze_body_type_facts("body_type_facts_inner_span", source, inner).coverage_gaps[0].kind,
        BodyTypeCoverageGapKind::BodyNotFound
    );
}

#[test]
fn body_range_cannot_select_appended_synthetic_declarations() {
    ready();
    let source = "fn exact() -> i32 { 7 }";
    for body in [
        TextRange { start: source.len() as u32, end: source.len() as u32 + 100 },
        TextRange { start: 0, end: 0 },
    ] {
        let facts = analyze_body_type_facts("body_type_facts_out_of_source", source, body);
        assert!(facts.expressions.is_empty());
        assert!(facts.body_span.is_none());
        assert_eq!(facts.typeck_tainted_by_errors, None);
        assert_eq!(facts.coverage_gaps[0].kind, BodyTypeCoverageGapKind::BodyNotFound);
    }
}

#[test]
fn nested_closure_body_is_an_explicit_coverage_gap() {
    ready();
    let source = "fn outer() -> i32 { let add = |x: i32| x + 1; add(2) }";
    let body = range(source, "{ let add = |x: i32| x + 1; add(2) }");
    let facts = analyze_body_type_facts("body_type_facts_closure", source, body);
    assert!(facts.coverage_gaps.iter().any(|gap| {
        gap.kind == BodyTypeCoverageGapKind::NestedBodyNotVisited && gap.span.is_some()
    }));
}

#[test]
fn symbolic_const_terms_are_preserved_as_an_explicit_gap_without_interpretation() {
    ready();
    let source = "const N: usize = 4; const M: usize = N * 2; fn symbolic() { takes_four([0; N]); let _: [u8; M] = [0; M]; takes_eight([0; M]); } fn takes_four(_: [u8; 4]) {} fn takes_eight(_: [u8; 8]) {}";
    let body =
        range(source, "{ takes_four([0; N]); let _: [u8; M] = [0; M]; takes_eight([0; M]); }");
    let before = frontend::rustc_const_eval::interp_cx_new_count();
    let facts = analyze_body_type_facts("body_type_facts_symbolic_consts", source, body);
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
    assert!(facts.uncomputed_consts.iter().any(|path| path.ends_with("M")));
    assert!(
        facts
            .coverage_gaps
            .iter()
            .any(|gap| { gap.kind == BodyTypeCoverageGapKind::NontrivialConstNotComputed })
    );
}

#[test]
fn for_lowering_exports_callsite_bound_typed_facts() {
    ready();
    let source = "struct One; struct OneIter; impl IntoIterator for One { type Item = i32; type IntoIter = OneIter; fn into_iter(self) -> OneIter { OneIter } } impl Iterator for OneIter { type Item = i32; fn next(&mut self) -> Option<i32> { None } } fn body() { for item in One { let typed: i32 = item; } }";
    let body = range(source, "{ for item in One { let typed: i32 = item; } }");
    let facts = analyze_body_type_facts("body_type_facts_for", source, body);
    assert!(facts.type_extraction_complete, "{facts:?}");
    assert!(facts.coverage_gaps.is_empty(), "{facts:?}");
    assert!(!facts.generated_expressions.is_empty(), "{facts:?}");
    assert!(facts.generated_expressions.iter().all(|fact| {
        fact.desugaring_chain.contains(&frontend::frontend_facts::BodyTypeDesugaringKind::ForLoop)
            && fact.callsite.start >= body.start
            && fact.callsite.end <= body.end
            && matches!(&fact.ty, StaticFact::Known { .. })
            && matches!(&fact.adjusted_ty, StaticFact::Known { .. })
    }));
    assert!(
        facts
            .projection_gaps
            .iter()
            .any(|gap| { gap.kind == BodyTypeCoverageGapKind::DesugaredExpression })
    );
}

#[test]
fn while_lowering_exports_callsite_bound_typed_facts() {
    ready();
    let source = "fn body() { while false { let typed: i32 = 1; } }";
    let body = range(source, "{ while false { let typed: i32 = 1; } }");
    let facts = analyze_body_type_facts("body_type_facts_while", source, body);
    assert!(facts.type_extraction_complete, "{facts:?}");
    assert!(facts.coverage_gaps.is_empty(), "{facts:?}");
    assert!(facts.generated_expressions.iter().any(|fact| {
        fact.desugaring_chain.contains(&frontend::frontend_facts::BodyTypeDesugaringKind::WhileLoop)
    }));
}

#[test]
fn nested_loop_lowerings_are_both_observed_in_one_exact_body() {
    ready();
    let source = "struct One; struct OneIter; impl IntoIterator for One { type Item = i32; type IntoIter = OneIter; fn into_iter(self) -> OneIter { OneIter } } impl Iterator for OneIter { type Item = i32; fn next(&mut self) -> Option<i32> { None } } fn body() { while false { for item in One { let typed: i32 = item; } } }";
    let body = range(source, "{ while false { for item in One { let typed: i32 = item; } } }");
    let facts = analyze_body_type_facts("body_type_facts_nested_loops", source, body);
    assert!(facts.type_extraction_complete, "{facts:?}");
    assert!(facts.coverage_gaps.is_empty(), "{facts:?}");
    assert!(facts.generated_expressions.iter().any(|fact| {
        fact.desugaring_chain.contains(&frontend::frontend_facts::BodyTypeDesugaringKind::WhileLoop)
    }));
    assert!(facts.generated_expressions.iter().any(|fact| {
        fact.desugaring_chain.contains(&frontend::frontend_facts::BodyTypeDesugaringKind::ForLoop)
    }));
}

#[test]
fn plain_match_is_source_owned_and_macro_mixed_with_loop_lowering_is_incomplete() {
    ready();
    let source = "macro_rules! generated_loop { () => { while false { let value: i32 = 1; } } } fn matched(flag: bool) -> i32 { match flag { true => 1, false => 2 } } fn expanded() { generated_loop!() }";
    let match_body = range(source, "{ match flag { true => 1, false => 2 } }");
    let matched = analyze_body_type_facts("body_type_facts_match", source, match_body);
    assert!(matched.type_extraction_complete, "{matched:?}");
    assert!(matched.generated_expressions.is_empty(), "{matched:?}");
    assert!(matched.projection_gaps.is_empty(), "{matched:?}");
    let expanded_body = range(source, "{ generated_loop!() }");
    let expanded = analyze_body_type_facts("body_type_facts_macro_loop", source, expanded_body);
    assert!(!expanded.type_extraction_complete, "{expanded:?}");
    assert!(
        expanded
            .coverage_gaps
            .iter()
            .any(|gap| { gap.kind == BodyTypeCoverageGapKind::MacroExpansion })
    );
}

#[test]
fn call_item_resolution_distinguishes_direct_method_and_indirect_calls() {
    ready();
    let source = "struct Thing; impl Thing { fn method(&self) -> bool { true } } fn target() -> bool { true } fn direct() -> bool { target() } fn indirect() -> bool { let call: fn() -> bool = target; call() } fn method_call() -> bool { Thing.method() }";
    let direct =
        analyze_body_type_facts("body_type_facts_calls", source, range(source, "{ target() }"));
    assert!(direct.expressions.iter().any(|fact| matches!(
        fact.call_resolution.as_ref(),
        Some(frontend::frontend_facts::BodyCallResolutionFact::ResolvedDirectItem { .. })
    )));
    let indirect = analyze_body_type_facts(
        "body_type_facts_calls",
        source,
        range(source, "{ let call: fn() -> bool = target; call() }"),
    );
    assert!(indirect.expressions.iter().any(|fact| matches!(
        fact.call_resolution.as_ref(),
        Some(frontend::frontend_facts::BodyCallResolutionFact::Unknown { .. })
    )));
    let method = analyze_body_type_facts(
        "body_type_facts_calls",
        source,
        range(source, "{ Thing.method() }"),
    );
    assert!(method.expressions.iter().any(|fact| matches!(
        fact.call_resolution.as_ref(),
        Some(frontend::frontend_facts::BodyCallResolutionFact::SelectedMethodItem { .. })
    )));
}

#[test]
fn generic_range_forms_export_known_range_facts_without_type_name_claims() {
    ready();
    let source = "struct Indexed<T> { value: T } impl<T, I> Index<I> for Indexed<T> { type Output = T; fn index(&self, _index: I) -> &T { &self.value } } fn generic<T>(items: &Indexed<T>, start: usize, end: usize) { let from = &items[start..]; let bounded = &items[start..end]; let to = &items[..end]; let to_inclusive = &items[..=end]; let inclusive = &items[start..=end]; let full = &items[..]; let _ = (from, bounded, to, to_inclusive, inclusive, full); }";
    let body = range(
        source,
        "{ let from = &items[start..]; let bounded = &items[start..end]; let to = &items[..end]; let to_inclusive = &items[..=end]; let inclusive = &items[start..=end]; let full = &items[..]; let _ = (from, bounded, to, to_inclusive, inclusive, full); }",
    );
    let facts = analyze_body_type_facts("body_type_facts_generic_ranges", source, body);

    assert!(facts.type_extraction_complete, "{facts:?}");
    assert!(facts.coverage_gaps.is_empty(), "{facts:?}");
    assert!(
        facts
            .generated_expressions
            .iter()
            .all(|fact| { fact.callsite.start >= body.start && fact.callsite.end <= body.end })
    );
    for (statement, expression) in [
        ("let from = &items[start..];", "start.."),
        ("let bounded = &items[start..end];", "start..end"),
        ("let to = &items[..end];", "..end"),
        ("let to_inclusive = &items[..=end];", "..=end"),
        ("let inclusive = &items[start..=end];", "start..=end"),
        ("let full = &items[..];", ".."),
    ] {
        assert_generated_range_fact(&facts, source, statement, expression);
    }
}

#[test]
fn nested_for_loops_keep_generic_range_attribution_inside_the_selected_body() {
    ready();
    let source = "struct Indexed<T> { value: T } impl<T, I> Index<I> for Indexed<T> { type Output = T; fn index(&self, _index: I) -> &T { &self.value } } struct Outer; struct OuterIter; impl IntoIterator for Outer { type Item = Outer; type IntoIter = OuterIter; fn into_iter(self) -> OuterIter { OuterIter } } impl Iterator for OuterIter { type Item = Outer; fn next(&mut self) -> Option<Outer> { None } } fn nested<T>(items: &Indexed<T>, start: usize, end: usize) { for _outer in Outer { for _inner in Outer { let from = &items[start..]; let bounded = &items[start..end]; let to = &items[..end]; let to_inclusive = &items[..=end]; let inclusive = &items[start..=end]; let full = &items[..]; let _ = (from, bounded, to, to_inclusive, inclusive, full); } } }";
    let body = range(
        source,
        "{ for _outer in Outer { for _inner in Outer { let from = &items[start..]; let bounded = &items[start..end]; let to = &items[..end]; let to_inclusive = &items[..=end]; let inclusive = &items[start..=end]; let full = &items[..]; let _ = (from, bounded, to, to_inclusive, inclusive, full); } } }",
    );
    let facts = analyze_body_type_facts("body_type_facts_nested_generic_ranges", source, body);

    assert!(facts.type_extraction_complete, "{facts:?}");
    assert!(facts.coverage_gaps.is_empty(), "{facts:?}");
    assert!(
        facts
            .generated_expressions
            .iter()
            .filter(|fact| { fact.desugaring_chain.contains(&BodyTypeDesugaringKind::ForLoop) })
            .count()
            >= 2,
        "{facts:?}"
    );
    for (statement, expression) in [
        ("let from = &items[start..];", "start.."),
        ("let bounded = &items[start..end];", "start..end"),
        ("let to = &items[..end];", "..end"),
        ("let to_inclusive = &items[..=end];", "..=end"),
        ("let inclusive = &items[start..=end];", "start..=end"),
        ("let full = &items[..];", ".."),
    ] {
        assert_generated_range_fact(&facts, source, statement, expression);
    }
}

#[test]
fn unsupported_async_desugaring_remains_an_explicit_coverage_refusal() {
    ready();
    let source = "fn unsupported() { let future = async { 1 }; let _ = future; }";
    let body = range(source, "{ let future = async { 1 }; let _ = future; }");
    let facts = analyze_body_type_facts("body_type_facts_unsupported_desugaring", source, body);

    assert!(!facts.type_extraction_complete, "{facts:?}");
    assert!(
        facts
            .coverage_gaps
            .iter()
            .any(|gap| { gap.kind == BodyTypeCoverageGapKind::NestedBodyNotVisited }),
        "{facts:?}"
    );
}

#[test]
fn macro_generated_range_is_refused_with_a_macro_expansion_gap() {
    ready();
    let source = "struct Indexed<T> { value: T } impl<T, I> Index<I> for Indexed<T> { type Output = T; fn index(&self, _index: I) -> &T { &self.value } } macro_rules! generated_range { ($start:expr) => { $start.. }; } fn body<T>(items: &Indexed<T>, start: usize) { let _ = &items[generated_range!(start)]; }";
    let body = range(source, "{ let _ = &items[generated_range!(start)]; }");
    let facts = analyze_body_type_facts("body_type_facts_macro_generated_range", source, body);

    assert!(!facts.type_extraction_complete, "{facts:?}");
    assert!(
        facts
            .coverage_gaps
            .iter()
            .any(|gap| { gap.kind == BodyTypeCoverageGapKind::MacroExpansion }),
        "{facts:?}"
    );
    assert!(
        facts
            .generated_expressions
            .iter()
            .all(|fact| { !fact.desugaring_chain.contains(&BodyTypeDesugaringKind::RangeExpr) }),
        "macro-generated range was admitted: {facts:?}"
    );
}

#[test]
fn macro_produced_range_bound_is_refused_with_a_macro_expansion_gap() {
    ready();
    let source = "struct Indexed<T> { value: T } impl<T, I> Index<I> for Indexed<T> { type Output = T; fn index(&self, _index: I) -> &T { &self.value } } macro_rules! generated_bound { () => { 7usize }; } fn body<T>(items: &Indexed<T>, start: usize) { let _ = &items[start..generated_bound!()]; }";
    let body = range(source, "{ let _ = &items[start..generated_bound!()]; }");
    let facts = analyze_body_type_facts("body_type_facts_macro_range_bound", source, body);

    assert!(!facts.type_extraction_complete, "{facts:?}");
    assert!(
        facts
            .coverage_gaps
            .iter()
            .any(|gap| { gap.kind == BodyTypeCoverageGapKind::MacroExpansion }),
        "{facts:?}"
    );
    assert_eq!(facts.typeck_tainted_by_errors, Some(false), "{facts:?}");
    assert!(!facts.fatal, "{facts:?}");
}
