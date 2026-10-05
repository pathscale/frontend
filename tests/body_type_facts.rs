use frontend::frontend_facts::effects::{
    EnclosingBodySpanError, StaticFact, TextRange, enclosing_function_body_span,
};
use frontend::frontend_facts::{
    BodyTypeCoverageGapKind, Loaded, Severity, analyze_body_type_facts,
    analyze_body_type_facts_with_loaded,
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
    assert!(facts.projection_gaps.iter().any(|gap| {
        gap.kind == BodyTypeCoverageGapKind::DesugaredExpression
    }));
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
    assert!(expanded.coverage_gaps.iter().any(|gap| {
        gap.kind == BodyTypeCoverageGapKind::MacroExpansion
    }));
}

#[test]
fn call_item_resolution_distinguishes_direct_method_and_indirect_calls() {
    ready();
    let source = "struct Thing; impl Thing { fn method(&self) -> bool { true } } fn target() -> bool { true } fn direct() -> bool { target() } fn indirect() -> bool { let call: fn() -> bool = target; call() } fn method_call() -> bool { Thing.method() }";
    let direct = analyze_body_type_facts("body_type_facts_calls", source, range(source, "{ target() }"));
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
