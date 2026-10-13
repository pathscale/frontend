use frontend::frontend_facts::effects::{
    EnclosingBodySpanError, StaticFact, TextRange, enclosing_function_body_span,
};
use frontend::frontend_facts::{
    BodyCallableIdentityCoverageGapKind, BodyExpressionIdentityFact, BodyTypeCoverageGapKind,
    BodyTypeDesugaringKind, BodyTypeFacts, BodyTypeIdentityReport, CrateRead,
    DefinitionSourceAttestation, DefinitionSourceKind, Dependency, Loaded, ResolvedBoundRegionKind,
    ResolvedCallableGenericArgumentIdentity, ResolvedCallableGenericTypeIdentity,
    ResolvedCallableKind, ResolvedCallableSourceContext, ResolvedCallableTraitSelection,
    ResolvedLateRegionKind, ResolvedOptionVariantRole, ResolvedPrimitive,
    ResolvedReferenceMutability, ResolvedRegionIdentity, ResolvedTypeIdentity,
    ResolvedVariantIdentity, Severity, StableDefPathIdentity, analyze_body_type_facts,
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
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        std::fs::create_dir_all(&target_dir).expect("create or find Cargo target directory");
        let target_dir = target_dir.canonicalize().expect("canonical Cargo target directory");
        let scratch_parent = target_dir.join("tmp");
        std::fs::create_dir_all(&scratch_parent)
            .expect("create test scratch parent under Cargo target");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        for attempt in 0u32.. {
            let path = scratch_parent.join(format!(
                "body-type-identity-{name}-{}-{nanos}-{attempt}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create owned test scratch directory: {error}"),
            }
        }
        unreachable!("an unbounded unique-directory counter is exhausted")
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
    let shadowed = "enum Option<T> { Some(T), None } struct Wrapper<T>(T); enum Shape { Record { value: u8 } } fn body() { let selected = Option::Some(1u8); let absent: Option<u8> = Option::None; let wrapped8 = Wrapper(1u8); let wrapped16 = Wrapper(1u16); let record = Shape::Record { value: 1u8 }; let matched = match Option::Some(2u8) { Option::Some(value) => Option::Some(value), Option::None => Option::None }; selected; absent; wrapped8; wrapped16; record; matched; }";
    let body = range(
        shadowed,
        "{ let selected = Option::Some(1u8); let absent: Option<u8> = Option::None; let wrapped8 = Wrapper(1u8); let wrapped16 = Wrapper(1u16); let record = Shape::Record { value: 1u8 }; let matched = match Option::Some(2u8) { Option::Some(value) => Option::Some(value), Option::None => Option::None }; selected; absent; wrapped8; wrapped16; record; matched; }",
    );
    let report = analyze_body_type_identity("body_identity_shadowed_option", shadowed, body);
    assert!(!report.type_facts.fatal, "{:?}", report.type_facts);
    assert!(
        !report.identity_facts.extraction_complete,
        "unsupported non-variant function-item identities stay explicit: {:?}",
        report.identity_facts
    );
    assert!(
        report
            .identity_facts
            .coverage_gaps
            .iter()
            .any(|gap| gap.kind == BodyTypeCoverageGapKind::ExpressionTypeMissing),
        "{:?}",
        report.identity_facts
    );
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
        StaticFact::Known {
            value: ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::U8 }
        }
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
    assert!(report.identity_facts.pattern_extraction_complete, "{:?}", report.identity_facts);
    assert!(report.identity_facts.pattern_coverage_gaps.is_empty(), "{:?}", report.identity_facts);
    let wire = serde_json::to_value(&report.identity_facts).expect("identity report serializes");
    assert_eq!(wire["pattern_coverage_gaps"].as_array().map(Vec::len), Some(0));
    assert!(wire["coverage_gaps"].is_array());
    assert!(wire["expressions"].is_array());
    assert!(wire["patterns"].is_array());
    assert_eq!(report.identity_facts.patterns.len(), 2, "{:?}", report.identity_facts.patterns);
    assert!(report.identity_facts.patterns.iter().all(|pattern| matches!(
        &pattern.resolved_variant,
        StaticFact::Known { value: Some(ResolvedVariantIdentity { option_role: None, .. }) }
    )));
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
    let injected_report =
        analyze_body_type_identity("body_identity_injected_option", injected, injected_body);
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
    assert!(
        injected_report
            .identity_facts
            .coverage_gaps
            .iter()
            .any(|gap| { gap.kind == BodyTypeCoverageGapKind::ExpressionTypeMissing })
    );

    let injected_patterns =
        "fn body(value: Option<u8>) -> u8 { match value { Some(item) => item, None => 0 } }";
    let injected_patterns_body =
        range(injected_patterns, "{ match value { Some(item) => item, None => 0 } }");
    let injected_patterns_report = analyze_body_type_identity(
        "body_identity_injected_option_patterns",
        injected_patterns,
        injected_patterns_body,
    );
    assert_eq!(injected_patterns_report.identity_facts.patterns.len(), 2);
    assert!(!injected_patterns_report.identity_facts.pattern_extraction_complete);
    assert!(!injected_patterns_report.identity_facts.extraction_complete);
    assert!(
        injected_patterns_report
            .identity_facts
            .pattern_coverage_gaps
            .iter()
            .any(|gap| {
                gap.kind
                    == frontend::frontend_facts::BodyPatternIdentityCoverageGapKind::PatternResolutionUnavailable
            })
    );
    assert!(injected_patterns_report.identity_facts.patterns.iter().all(|pattern| matches!(
        &pattern.resolved_variant,
        StaticFact::Unknown { why } if why == "synthetic_lang_item_context"
    )));
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
    let source = "#![feature(no_core)]\n#![no_core]\nextern crate optionlang;\nfn body(input: optionlang::Option<u32>) -> optionlang::Option<u32> { match input { optionlang::Option::Some(value) => optionlang::Option::Some(value), optionlang::Option::None => optionlang::Option::None } }";
    let body = range(
        source,
        "{ match input { optionlang::Option::Some(value) => optionlang::Option::Some(value), optionlang::Option::None => optionlang::Option::None } }",
    );
    let report = analyze_body_type_identity_with_loaded(
        "body_identity_loaded_option",
        source,
        body,
        Some("2024"),
        Loaded { dependencies: &dependencies, ..Loaded::default() },
    );
    assert!(report.identity_facts.pattern_extraction_complete, "{:?}", report.identity_facts);
    assert!(report.identity_facts.pattern_coverage_gaps.is_empty(), "{:?}", report.identity_facts);
    assert_eq!(report.identity_facts.patterns.len(), 2, "{:?}", report.identity_facts.patterns);
    assert!(report.identity_facts.extraction_complete, "{:?}", report.identity_facts);

    let some_pattern = "optionlang::Option::Some(value)";
    let some_start = source.find(some_pattern).expect("Some pattern") as u32;
    let some_end = some_start + some_pattern.len() as u32;
    let some_pattern_fact = report
        .identity_facts
        .patterns
        .iter()
        .find(|fact| fact.span.start == some_start && fact.span.end == some_end)
        .expect("source-mapped Some pattern identity");
    let StaticFact::Known { value: Some(some_variant) } = &some_pattern_fact.resolved_variant
    else {
        panic!("external Option Some pattern was not resolved: {some_pattern_fact:?}");
    };
    assert_eq!(some_variant.option_role, Some(ResolvedOptionVariantRole::Some));

    let none_pattern = "optionlang::Option::None";
    let none_start = source.find(none_pattern).expect("None pattern") as u32;
    let none_end = none_start + none_pattern.len() as u32;
    let none_pattern_fact = report
        .identity_facts
        .patterns
        .iter()
        .find(|fact| fact.span.start == none_start && fact.span.end == none_end)
        .expect("source-mapped None pattern identity");
    let StaticFact::Known { value: Some(none_variant) } = &none_pattern_fact.resolved_variant
    else {
        panic!("external Option None pattern was not resolved: {none_pattern_fact:?}");
    };
    assert_eq!(none_variant.option_role, Some(ResolvedOptionVariantRole::None));
    assert_eq!(&some_variant.parent_enum, &none_variant.parent_enum);
    assert_ne!(&some_variant.variant, &none_variant.variant);

    let input_marker = "match input";
    let input_start =
        (source.find(input_marker).expect("Option scrutinee") + "match ".len()) as u32;
    let input_end = input_start + "input".len() as u32;
    let input_expression = report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == input_start && fact.span.end == input_end)
        .expect("source-mapped Option scrutinee identity");
    let StaticFact::Known { value: ResolvedTypeIdentity::OptionLangItem { definition, argument } } =
        &input_expression.resolved_ty
    else {
        panic!("external Option scrutinee type was not resolved: {input_expression:?}");
    };
    assert!(matches!(
        argument.as_ref(),
        ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::U32 }
    ));
    assert_eq!(&some_variant.parent_enum, definition);
    assert_eq!(&none_variant.parent_enum, definition);

    let some_path = "optionlang::Option::Some";
    let some_path_start =
        source.match_indices(some_path).nth(1).expect("Some expression callee").0 as u32;
    let some_path_end = some_path_start + some_path.len() as u32;
    let some_callee = report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == some_path_start && fact.span.end == some_path_end)
        .expect("source-mapped Some constructor callee");
    let StaticFact::Known {
        value: ResolvedTypeIdentity::VariantConstructor { variant, arguments },
    } = &some_callee.resolved_ty
    else {
        panic!("external Some constructor identity was not resolved: {some_callee:?}");
    };
    assert_eq!(variant, some_variant);
    assert_eq!(arguments, &[ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::U32 }]);
    assert!(matches!(
        &some_callee.resolved_variant,
        StaticFact::Known { value: Some(identity) } if identity == some_variant
    ));

    let tuple_source = "#![feature(no_core)]\n#![no_core]\nextern crate optionlang;\nfn body(left: &'static str, right: &'static str) -> optionlang::Option<(&'static str, &'static str)> { optionlang::Option::Some((left, right)) }";
    let tuple_body = range(tuple_source, "{ optionlang::Option::Some((left, right)) }");
    let tuple_report = analyze_body_type_identity_with_loaded(
        "body_identity_loaded_option_tuple_refs",
        tuple_source,
        tuple_body,
        Some("2024"),
        Loaded { dependencies: &dependencies, ..Loaded::default() },
    );
    assert!(tuple_report.identity_facts.extraction_complete, "{:?}", tuple_report.identity_facts);
    let tuple_call_text = "optionlang::Option::Some((left, right))";
    let tuple_call_start = tuple_source.find(tuple_call_text).expect("tuple Option call") as u32;
    let tuple_call_end = tuple_call_start + tuple_call_text.len() as u32;
    let tuple_call = tuple_report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == tuple_call_start && fact.span.end == tuple_call_end)
        .expect("full source span for tuple Option call");
    // HIR typeck writeback erases body regions before these expression types are read.
    // Keep the compiler-reported erasure; the source's `'static` tokens do not survive here.
    let expected_tuple = ResolvedTypeIdentity::Tuple {
        elements: vec![
            ResolvedTypeIdentity::Reference {
                mutability: ResolvedReferenceMutability::Shared,
                region: ResolvedRegionIdentity::Erased,
                referent: Box::new(ResolvedTypeIdentity::Primitive {
                    primitive: ResolvedPrimitive::Str,
                }),
            },
            ResolvedTypeIdentity::Reference {
                mutability: ResolvedReferenceMutability::Shared,
                region: ResolvedRegionIdentity::Erased,
                referent: Box::new(ResolvedTypeIdentity::Primitive {
                    primitive: ResolvedPrimitive::Str,
                }),
            },
        ],
    };
    assert!(
        matches!(
            &tuple_call.resolved_ty,
            StaticFact::Known {
                value: ResolvedTypeIdentity::OptionLangItem { argument, .. }
            } if argument.as_ref() == &expected_tuple
        ),
        "body expression type preserves the tuple shape but not source lifetimes: {tuple_call:?}"
    );
    assert!(matches!(
        &tuple_call.resolved_callable,
        Some(StaticFact::Known {
            value: Some(callable)
        }) if callable.kind == ResolvedCallableKind::Constructor
    ));
}

#[test]
fn ordinary_function_items_have_definition_type_identities() {
    ready();
    let source = "fn ordinary() -> u8 { 1 } fn body() -> u8 { ordinary() }";
    let body = range(source, "{ ordinary() }");
    let report = analyze_body_type_identity("body_identity_ordinary_function", source, body);
    let path_start = source.rfind("ordinary").expect("ordinary function callee") as u32;
    let path_end = path_start + "ordinary".len() as u32;
    let callee = report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == path_start && fact.span.end == path_end)
        .expect("source-mapped ordinary function item");
    let StaticFact::Known { value: ResolvedTypeIdentity::FunctionItem { definition, arguments } } =
        &callee.resolved_ty
    else {
        panic!("ordinary function item has no typed definition identity: {callee:?}");
    };
    assert!(arguments.is_empty());
    let call_start = source.rfind("ordinary()").expect("call expression") as u32;
    let call_end = call_start + "ordinary()".len() as u32;
    let call = report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == call_start && fact.span.end == call_end)
        .expect("full call expression identity");
    assert!(matches!(
        &call.resolved_callable,
        Some(StaticFact::Known {
            value: Some(callable)
        }) if callable.kind == ResolvedCallableKind::DirectItem && callable.definition == *definition
    ));
    assert!(matches!(&callee.resolved_callable, Some(StaticFact::Known { value: None })));
    assert!(report.identity_facts.callable_extraction_complete);
    assert!(report.identity_facts.callable_coverage_gaps.is_empty());
    let mut legacy_identity_wire =
        serde_json::to_value(&report.identity_facts).expect("identity facts serialize");
    let legacy_object =
        legacy_identity_wire.as_object_mut().expect("identity facts wire is an object");
    legacy_object.remove("callable_extraction_complete");
    legacy_object.remove("callable_coverage_gaps");
    for expression in legacy_object
        .get_mut("expressions")
        .and_then(serde_json::Value::as_array_mut)
        .expect("identity rows serialize as an array")
    {
        expression
            .as_object_mut()
            .expect("expression fact is an object")
            .remove("resolved_callable");
    }
    let legacy_facts: frontend::frontend_facts::BodyTypeIdentityFacts =
        serde_json::from_value(legacy_identity_wire).expect("legacy identity facts parse");
    assert!(!legacy_facts.callable_extraction_complete);
    assert!(legacy_facts.callable_coverage_gaps.is_empty());
    assert!(legacy_facts.expressions.iter().all(|fact| fact.resolved_callable.is_none()));
    assert!(report.identity_facts.extraction_complete);
    assert!(report.identity_facts.coverage_gaps.is_empty());
}

fn identity_expression_at<'a>(
    report: &'a BodyTypeIdentityReport,
    source: &str,
    snippet: &str,
) -> &'a BodyExpressionIdentityFact {
    let start = source.find(snippet).expect("identity source snippet") as u32;
    report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| {
            fact.span.start == start
                && matches!(
                    &fact.resolved_ty,
                    StaticFact::Known { value: ResolvedTypeIdentity::FunctionItem { .. } }
                        | StaticFact::Unknown { .. }
                )
        })
        .expect("source-mapped identity expression")
}

fn identity_expression_for_range<'a>(
    report: &'a BodyTypeIdentityReport,
    source: &str,
    snippet: &str,
) -> &'a BodyExpressionIdentityFact {
    let start = source.find(snippet).expect("identity source snippet") as u32;
    let end = start + snippet.len() as u32;
    report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == start && fact.span.end == end)
        .expect("source-mapped identity expression with exact range")
}

fn function_item_identity(
    fact: &BodyExpressionIdentityFact,
) -> (&StableDefPathIdentity, &[ResolvedTypeIdentity]) {
    match &fact.resolved_ty {
        StaticFact::Known {
            value: ResolvedTypeIdentity::FunctionItem { definition, arguments },
        } => (definition, arguments),
        other => panic!("expected typed function item identity, got {other:?}"),
    }
}

#[test]
fn function_item_identity_distinguishes_definitions_and_type_arguments() {
    ready();
    let same_named_source = "mod left { pub fn same() {} } mod right { pub fn same() {} } fn body() { left::same(); right::same(); }";
    let same_named_report = analyze_body_type_identity(
        "body_identity_same_named_functions",
        same_named_source,
        range(same_named_source, "{ left::same(); right::same(); }"),
    );
    let left_item = identity_expression_at(&same_named_report, same_named_source, "left::same");
    let right_item = identity_expression_at(&same_named_report, same_named_source, "right::same");
    let (left_definition, left_arguments) = function_item_identity(left_item);
    let (right_definition, right_arguments) = function_item_identity(right_item);
    assert_ne!(left_definition, right_definition);
    assert!(left_arguments.is_empty() && right_arguments.is_empty());
    for (path, call) in [("left::same", "left::same()"), ("right::same", "right::same()")] {
        let item = function_item_identity(identity_expression_at(
            &same_named_report,
            same_named_source,
            path,
        ));
        let call_fact = identity_expression_for_range(&same_named_report, same_named_source, call);
        assert!(matches!(
            &call_fact.resolved_callable,
            Some(StaticFact::Known { value: Some(callable) })
                if callable.definition == *item.0
        ));
    }

    let generic_source = "fn generic<T>() {} fn body() { generic::<u8>(); generic::<u16>(); }";
    let generic_report = analyze_body_type_identity(
        "body_identity_generic_function_items",
        generic_source,
        range(generic_source, "{ generic::<u8>(); generic::<u16>(); }"),
    );
    let u8_item = identity_expression_at(&generic_report, generic_source, "generic::<u8>");
    let u16_item = identity_expression_at(&generic_report, generic_source, "generic::<u16>");
    let (u8_definition, u8_arguments) = function_item_identity(u8_item);
    let (u16_definition, u16_arguments) = function_item_identity(u16_item);
    assert_eq!(u8_definition, u16_definition);
    assert_eq!(
        u8_arguments,
        &[ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::U8 }]
    );
    assert_eq!(
        u16_arguments,
        &[ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::U16 }]
    );

    let const_source = "fn const_generic<const N: usize>() {} fn body() { const_generic::<3>(); }";
    let const_report = analyze_body_type_identity(
        "body_identity_const_generic_function_item",
        const_source,
        range(const_source, "{ const_generic::<3>(); }"),
    );
    assert!(matches!(
        &identity_expression_at(&const_report, const_source, "const_generic::<3>").resolved_ty,
        StaticFact::Unknown { why } if why == "non_type_generic_argument_identity_unavailable"
    ));

    let unresolved_source = "fn generic<T>() {} fn body() { generic(); }";
    let unresolved_report = analyze_body_type_identity(
        "body_identity_unresolved_generic_function_item",
        unresolved_source,
        range(unresolved_source, "{ generic(); }"),
    );
    assert!(matches!(
        &identity_expression_at(&unresolved_report, unresolved_source, "generic()").resolved_ty,
        StaticFact::Unknown { .. }
    ));

    let bound_source = "fn generic<T>() {} fn body() { generic::<for<'a> fn(&'a u8)>(); }";
    let bound_report = analyze_body_type_identity(
        "body_identity_bound_function_item_argument",
        bound_source,
        range(bound_source, "{ generic::<for<'a> fn(&'a u8)>(); }"),
    );
    assert!(matches!(
        &identity_expression_at(&bound_report, bound_source, "generic::<for<'a>").resolved_ty,
        StaticFact::Unknown { .. }
    ));
}

#[test]
fn tuple_and_reference_type_identities_are_structural_and_bounded() {
    ready();
    let source = "fn tuple(left: &'static str, right: &'static str) -> (&'static str, &'static str) { (left, right) } fn mutable(value: &'static mut u8) -> &'static mut u8 { value } fn raw(value: *const u8) -> *const u8 { value }";
    let tuple_report = analyze_body_type_identity(
        "body_identity_tuple_refs",
        source,
        range(source, "{ (left, right) }"),
    );
    assert!(tuple_report.identity_facts.extraction_complete, "{:?}", tuple_report.identity_facts);
    let tuple_expr = "(left, right)";
    let tuple_start = source.find(tuple_expr).expect("tuple expression") as u32;
    let tuple_end = tuple_start + tuple_expr.len() as u32;
    let tuple_fact = tuple_report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == tuple_start && fact.span.end == tuple_end)
        .expect("source tuple type identity");
    let expected_ref = ResolvedTypeIdentity::Reference {
        mutability: ResolvedReferenceMutability::Shared,
        region: ResolvedRegionIdentity::Erased,
        referent: Box::new(ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::Str }),
    };
    // rustc_hir_typeck/writeback.rs deliberately folds body expression regions to `ReErased`.
    // These source annotations therefore prove the reference shape, not `'static` identity.
    assert!(
        matches!(
            &tuple_fact.resolved_ty,
            StaticFact::Known {
                value: ResolvedTypeIdentity::Tuple { elements }
            } if elements == &[expected_ref.clone(), expected_ref]
        ),
        "body typeck fact should preserve structure and explicit erasure: {tuple_fact:?}"
    );

    let mutable_start = source.find("fn mutable").expect("mutable function") as u32;
    let mutable_body_start = mutable_start
        + source[mutable_start as usize..].find("{ value }").expect("mutable function body") as u32;
    let mutable_report = analyze_body_type_identity(
        "body_identity_mutable_ref",
        source,
        TextRange { start: mutable_body_start, end: mutable_body_start + "{ value }".len() as u32 },
    );
    let value_start = mutable_body_start + "{ ".len() as u32;
    let value_fact = mutable_report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == value_start && fact.span.end == value_start + 5)
        .expect("mutable reference expression identity");
    assert!(
        matches!(
            &value_fact.resolved_ty,
            StaticFact::Known {
                value: ResolvedTypeIdentity::Reference {
                    mutability: ResolvedReferenceMutability::Mutable,
                    region: ResolvedRegionIdentity::Erased,
                    referent,
                }
            } if matches!(
                referent.as_ref(),
                ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::U8 }
            )
        ),
        "mutable reference type should preserve mutability while reporting erased lifetime: {value_fact:?}"
    );

    let raw_start = source.find("fn raw").expect("raw pointer function") as u32;
    let raw_body_start = raw_start
        + source[raw_start as usize..].find("{ value }").expect("raw pointer function body") as u32;
    let raw_report = analyze_body_type_identity(
        "body_identity_raw_pointer_unknown",
        source,
        TextRange { start: raw_body_start, end: raw_body_start + "{ value }".len() as u32 },
    );
    let raw_value_start = raw_body_start + "{ ".len() as u32;
    let raw_fact = raw_report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == raw_value_start && fact.span.end == raw_value_start + 5)
        .expect("raw pointer expression identity");
    assert!(matches!(
        &raw_fact.resolved_ty,
        StaticFact::Unknown { why } if why == "unsupported_resolved_type_form"
    ));
    assert!(!raw_report.identity_facts.extraction_complete);
}

#[test]
fn body_expression_lifetime_identity_reports_typeck_erasure_not_source_spelling() {
    ready();
    let source = "fn body<'a>(value: &'a str) -> &'a str { value }";
    let report = analyze_body_type_identity(
        "body_identity_named_lifetime",
        source,
        range(source, "{ value }"),
    );
    let value_start = source.find("{ value }").expect("body value") as u32 + "{ ".len() as u32;
    let value_fact = report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == value_start && fact.span.end == value_start + 5)
        .expect("source-mapped body value identity");
    assert!(
        matches!(
            &value_fact.resolved_ty,
            StaticFact::Known {
                value: ResolvedTypeIdentity::Reference {
                    mutability: ResolvedReferenceMutability::Shared,
                    region: ResolvedRegionIdentity::Erased,
                    referent,
                }
            } if matches!(
                referent.as_ref(),
                ResolvedTypeIdentity::Primitive { primitive: ResolvedPrimitive::Str }
            )
        ),
        "the body typeck result erased `'a`; source spelling is not lifetime identity: {value_fact:?}"
    );
}

#[test]
fn callable_identity_tracks_direct_method_and_refuses_indirect_targets() {
    ready();
    let source = "struct Splitter; impl Splitter { fn split_once(&self, value: u8) -> u8 { value } } fn target() -> u8 { 1 } fn direct() -> u8 { target() } fn method(value: u8) -> u8 { Splitter.split_once(value) } fn ufcs(value: u8) -> u8 { Splitter::split_once(&Splitter, value) } fn indirect(call: fn() -> u8) -> u8 { call() }";

    // Stable local DefPath identity includes the crate identity. Use one exact crate name so
    // this comparison tests method versus UFCS resolution, not unrelated crate-local keys.
    let direct = analyze_body_type_identity(
        "body_identity_callable_modes",
        source,
        range(source, "{ target() }"),
    );
    let direct_call = direct
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == source.rfind("target()").expect("direct call") as u32)
        .expect("direct call expression identity");
    assert!(matches!(
        &direct_call.resolved_callable,
        Some(StaticFact::Known {
            value: Some(callable)
        }) if callable.kind == ResolvedCallableKind::DirectItem
    ));
    let direct_wire = serde_json::to_value(direct_call).expect("call identity serializes");
    let mut legacy_wire = direct_wire;
    legacy_wire.as_object_mut().expect("expression fact is an object").remove("resolved_callable");
    let legacy: frontend::frontend_facts::BodyExpressionIdentityFact =
        serde_json::from_value(legacy_wire).expect("legacy fact without call identity parses");
    assert_eq!(
        legacy.resolved_callable, None,
        "legacy omission is unavailable, not known non-call"
    );

    let method = analyze_body_type_identity(
        "body_identity_callable_modes",
        source,
        range(source, "{ Splitter.split_once(value) }"),
    );
    let method_call = method
        .identity_facts
        .expressions
        .iter()
        .find(|fact| {
            fact.span.start
                == source.find("Splitter.split_once(value)").expect("method call") as u32
        })
        .expect("full method call expression identity");
    let StaticFact::Known { value: Some(method_identity) } =
        method_call.resolved_callable.as_ref().expect("new response includes callable status")
    else {
        panic!("method selection was not resolved: {method_call:?}");
    };
    assert_eq!(method_identity.kind, ResolvedCallableKind::SelectedMethod);

    let ufcs = analyze_body_type_identity(
        "body_identity_callable_modes",
        source,
        range(source, "{ Splitter::split_once(&Splitter, value) }"),
    );
    let ufcs_start =
        source.find("Splitter::split_once(&Splitter, value)").expect("UFCS call") as u32;
    let ufcs_call = ufcs
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == ufcs_start)
        .expect("UFCS call expression identity");
    let StaticFact::Known { value: Some(ufcs_identity) } =
        ufcs_call.resolved_callable.as_ref().expect("new response includes callable status")
    else {
        panic!("UFCS selection was not resolved: {ufcs_call:?}");
    };
    assert_eq!(method_identity.definition, ufcs_identity.definition);

    let indirect = analyze_body_type_identity(
        "body_identity_callable_modes",
        source,
        range(source, "{ call() }"),
    );
    let indirect_call = indirect
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == source.rfind("call()").expect("indirect call") as u32)
        .expect("indirect call expression identity");
    assert!(
        matches!(
            &indirect_call.resolved_callable,
            Some(StaticFact::Unknown { why }) if why == "indirect_call_target"
        ),
        "a local function-pointer path has no selected item identity: {indirect_call:?}"
    );
    assert!(
        indirect
            .identity_facts
            .callable_coverage_gaps
            .iter()
            .any(|gap| { gap.kind == BodyCallableIdentityCoverageGapKind::TargetUnavailable })
    );
    assert!(!indirect.identity_facts.callable_extraction_complete);
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
fn nested_closure_patterns_are_covered_with_strict_identity_refusals() {
    ready();
    let source = "enum Choice { Some(u8), None } fn outer(input: Choice) -> i32 { let classify = || match input { Choice::Some(value) => value as i32, Choice::None => 0 }; classify() }";
    let body = range(
        source,
        "{ let classify = || match input { Choice::Some(value) => value as i32, Choice::None => 0 }; classify() }",
    );
    let report = analyze_body_type_identity("body_identity_nested_patterns", source, body);
    assert!(report.type_facts.type_extraction_complete, "{:?}", report.type_facts);
    assert!(report.type_facts.coverage_gaps.is_empty(), "{:?}", report.type_facts);
    assert_eq!(report.type_facts.typeck_tainted_by_errors, Some(false), "{:?}", report.type_facts);
    assert!(!report.type_facts.fatal, "{:?}", report.type_facts);
    assert!(report.identity_facts.pattern_extraction_complete, "{:?}", report.identity_facts);
    assert!(report.identity_facts.pattern_coverage_gaps.is_empty(), "{:?}", report.identity_facts);
    assert_eq!(report.identity_facts.patterns.len(), 2, "{:?}", report.identity_facts.patterns);
    let pattern_at = |text: &str| {
        let expected = range(source, text);
        let fact = report
            .identity_facts
            .patterns
            .iter()
            .find(|fact| fact.span.start == expected.start && fact.span.end == expected.end)
            .expect("exact source-mapped closure pattern");
        match &fact.resolved_variant {
            StaticFact::Known { value: Some(variant) } => variant,
            other => panic!("closure pattern {text} lacks a variant identity: {other:?}"),
        }
    };
    let some = pattern_at("Choice::Some(value)");
    let none = pattern_at("Choice::None");
    assert_eq!(some.parent_enum, none.parent_enum);
    assert_ne!(some.variant, none.variant);
    assert_eq!(some.option_role, None);
    assert_eq!(none.option_role, None);
    let closure =
        range(source, "|| match input { Choice::Some(value) => value as i32, Choice::None => 0 }");
    assert!(
        report.identity_facts.expressions.iter().any(|fact| {
            fact.span.start == closure.start
                && fact.span.end == closure.end
                && matches!(
                    &fact.resolved_ty,
                    StaticFact::Unknown { why } if why == "unsupported_resolved_type_form"
                )
        }),
        "{:?}",
        report.identity_facts
    );
    assert!(
        report.identity_facts.coverage_gaps.iter().any(|gap| {
            gap.kind == BodyTypeCoverageGapKind::ExpressionTypeMissing
                && gap
                    .span
                    .as_ref()
                    .is_some_and(|span| span.start == closure.start && span.end == closure.end)
        }),
        "{:?}",
        report.identity_facts
    );
    assert!(
        !report.identity_facts.extraction_complete,
        "unsupported closure identities remain an explicit refusal: {:?}",
        report.identity_facts
    );
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

#[test]
fn region_identity_nested_kind_fields_serialize_without_colliding_with_variant_tag() {
    let bound = ResolvedRegionIdentity::Bound {
        debruijn: 1,
        variable: 2,
        region_kind: ResolvedBoundRegionKind::Anonymous,
    };
    let bound_wire = serde_json::to_value(bound).expect("bound region identity serializes");
    assert_eq!(bound_wire["kind"], "bound");
    assert_eq!(bound_wire["debruijn"], 1);
    assert_eq!(bound_wire["variable"], 2);
    assert_eq!(bound_wire["region_kind"]["kind"], "anonymous");

    let late_param = ResolvedRegionIdentity::LateParam {
        scope: StableDefPathIdentity {
            scheme: "test_def_path_hash_v1".to_string(),
            stable_crate_id: "0000000000000000".to_string(),
            item_path_hash: "1111111111111111".to_string(),
        },
        region_kind: ResolvedLateRegionKind::Anonymous { index: 3 },
    };
    let late_param_wire =
        serde_json::to_value(late_param).expect("late region identity serializes");
    assert_eq!(late_param_wire["kind"], "late_param");
    assert_eq!(late_param_wire["scope"]["scheme"], "test_def_path_hash_v1");
    assert_eq!(late_param_wire["region_kind"]["kind"], "anonymous");
    assert_eq!(late_param_wire["region_kind"]["index"], 3);
}

fn selected_trait_inputs(name: &str) -> (Scratch, std::path::PathBuf, [Dependency; 1]) {
    let roots = std::env::var("FRONTEND_RUST_SRC_ROOTS")
        .expect("set the exact pinned rust-src root for this ignored test");
    let roots: Vec<_> = roots.split(':').filter(|root| !root.is_empty()).collect();
    assert_eq!(roots.len(), 1, "select exactly one configured rust-src root");
    let source_root = std::path::PathBuf::from(roots[0]).canonicalize().unwrap();
    let pattern_path = source_root.join("library/core/src/str/pattern.rs");
    let split_path = source_root.join("library/core/src/str/mod.rs");
    let sha256 = |path: &std::path::Path| {
        use sha2::Digest as _;
        sha2::Sha256::digest(std::fs::read(path).expect("read configured pinned rust-src"))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    assert_eq!(
        sha256(&pattern_path),
        "f847b790c8c3575969d8cf30044d7bd6d6f94810260002da543a42c2f0a8448c"
    );
    assert_eq!(
        sha256(&split_path),
        "2c90ff097999f139148ef065c428c35a07807d8b38535ce5e2b43d65ad0deaae"
    );

    let scratch = Scratch::new(name);
    let core_root = source_root.join("library/core/src/lib.rs");
    let core_metadata = scratch.0.join("libcore.rmeta");
    let core_root_text = core_root.to_str().expect("configured core root is UTF-8");
    let core_metadata_text = core_metadata.to_str().expect("test-owned metadata path is UTF-8");
    let core_cfg = Vec::new();
    let core_env = vec![
        ("CARGO_PKG_NAME".to_string(), "core".to_string()),
        ("CARGO_PKG_VERSION".to_string(), "0.0.0".to_string()),
        ("CARGO_PKG_VERSION_MAJOR".to_string(), "0".to_string()),
        ("CARGO_PKG_VERSION_MINOR".to_string(), "0".to_string()),
        ("CARGO_PKG_VERSION_PATCH".to_string(), "0".to_string()),
        ("CARGO_PKG_VERSION_PRE".to_string(), String::new()),
        ("CARGO_CRATE_NAME".to_string(), "core".to_string()),
        (
            "CARGO_MANIFEST_DIR".to_string(),
            source_root.join("library/core").to_string_lossy().into_owned(),
        ),
    ];
    let core = read_crate(
        &CrateRead {
            edition: Some("2024"),
            items_only: true,
            standard_library: true,
            loaded: Loaded { cfg: &core_cfg, env: &core_env, ..Loaded::default() },
            write_metadata: Some(eko::path::Path::new(core_metadata_text)),
            ..CrateRead::new("core", eko::path::Path::new(core_root_text))
        }
        .library(true)
        .with_disambiguator("frontend-body-type-facts-core-source-v1"),
    )
    .expect("the same frontend build reads configured core source into metadata");
    assert!(
        core.complete,
        "source-derived core facts are incomplete; diagnostics: {:?}",
        core.diagnostics.iter().take(12).collect::<Vec<_>>()
    );
    assert!(
        core.definitions.iter().any(
            |definition| definition.name == "split_once" && definition.def_path.contains("str")
        ),
        "configured core source did not yield str::split_once: {:?}",
        core.definitions
            .iter()
            .filter(|definition| definition.name == "split_once")
            .collect::<Vec<_>>()
    );
    assert!(core_metadata.is_file(), "same-build core metadata was not written");
    let metadata = core_metadata.to_str().unwrap().to_owned();
    let deps = [Dependency::new("core", metadata)];
    (scratch, source_root, deps)
}

fn normalized_source(path: &std::path::Path) -> (std::path::PathBuf, String) {
    let path = path.canonicalize().expect("configured source file exists");
    let text = String::from_utf8(std::fs::read(&path).expect("read configured source file"))
        .expect("pinned rust-src is UTF-8");
    (path, text.strip_prefix('\u{feff}').unwrap_or(&text).replace("\r\n", "\n"))
}

fn checked_source_slice(
    path: &std::path::Path,
    span: &frontend::frontend_facts::ByteSpan,
    whole_digest: &str,
    item_digest: &str,
    source: &str,
) -> String {
    assert_eq!(
        std::path::Path::new(span.file.as_ref()).canonicalize().unwrap(),
        path.canonicalize().unwrap(),
        "attestation must name this exact configured file"
    );
    let digest = |bytes: &[u8]| format!("blake3={}", blake3::hash(bytes).to_hex());
    assert_eq!(whole_digest, digest(source.as_bytes()));
    let item = source
        .get(span.start as usize..span.end as usize)
        .expect("attested span is inside normalized source bytes");
    assert_eq!(item_digest, digest(item.as_bytes()));
    item.to_owned()
}

fn checked_definition(
    path: &std::path::Path,
    source: &str,
    attestation: &DefinitionSourceAttestation,
    kind: DefinitionSourceKind,
) -> String {
    assert_eq!(attestation.definition_kind, kind);
    assert_eq!(attestation.source_digest_scheme, "rustc_normalized_source_blake3_v1");
    checked_source_slice(
        path,
        &attestation.item_span,
        &attestation.source_content_digest,
        &attestation.item_digest,
        source,
    )
}

fn known<'a, T: std::fmt::Debug>(fact: &'a StaticFact<T>, label: &str) -> &'a T {
    match fact {
        StaticFact::Known { value } => value,
        StaticFact::Unknown { why } => panic!("{label} is Unknown: {why}"),
    }
}

fn callable_for(
    call: &BodyExpressionIdentityFact,
) -> &frontend::frontend_facts::ResolvedCallableIdentity {
    match call.resolved_callable.as_ref() {
        Some(StaticFact::Known { value: Some(callable) }) => callable,
        other => panic!("expected a compiler-resolved call identity: {other:?}"),
    }
}

fn call_fact<'a>(
    report: &'a frontend::frontend_facts::BodyTypeIdentityReport,
    source: &str,
    text: &str,
) -> &'a BodyExpressionIdentityFact {
    let start = source.rfind(text).expect("call text occurs in the analyzed source") as u32;
    let end = start + text.len() as u32;
    report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == start && fact.span.end == end)
        .unwrap_or_else(|| {
            panic!("no identity fact covers full call `{text}`: {:?}", report.identity_facts)
        })
}

fn known_type_argument(
    arg: &StaticFact<ResolvedCallableGenericArgumentIdentity>,
) -> Option<&ResolvedCallableGenericTypeIdentity> {
    match arg {
        StaticFact::Known { value: ResolvedCallableGenericArgumentIdentity::Type { value } } => {
            Some(value)
        }
        StaticFact::Known { .. } | StaticFact::Unknown { .. } => None,
    }
}

fn trait_ref(
    obligation: &frontend::frontend_facts::ResolvedCallableTraitObligation,
) -> Option<&frontend::frontend_facts::ResolvedCallableTraitRefIdentity> {
    match &obligation.trait_ref {
        StaticFact::Known { value } => Some(value),
        StaticFact::Unknown { .. } => None,
    }
}

fn trait_source(
    obligation: &frontend::frontend_facts::ResolvedCallableTraitObligation,
) -> Option<&DefinitionSourceAttestation> {
    match &obligation.trait_source_attestation {
        StaticFact::Known { value } => Some(value),
        StaticFact::Unknown { .. } => None,
    }
}

fn assert_core_context(
    actual: &ResolvedCallableSourceContext,
    expected: &ResolvedCallableSourceContext,
) {
    assert_eq!(actual, expected, "all linked core facts share one compiler/crate context");
    assert_eq!(actual.crate_name, "core");
}

fn has_user_defined_selection(
    callable: &frontend::frontend_facts::ResolvedCallableIdentity,
) -> bool {
    let Some(StaticFact::Known { value }) = callable.declared_trait_obligations.as_ref() else {
        return false;
    };
    value.iter().any(|obligation| {
        matches!(
            &obligation.selection,
            StaticFact::Known { value: ResolvedCallableTraitSelection::UserDefined { .. } }
        )
    })
}

#[test]
#[ignore = "requires the exact pinned rust-src source configured by FRONTEND_RUST_SRC_ROOTS; the test writes core metadata with this frontend build"]
fn concrete_char_split_once_proves_the_pattern_searcher_and_method_body_chain() {
    ready();
    let (_scratch, root, dependencies) = selected_trait_inputs("selected-char-pattern");
    let source = "#![no_std]\nextern crate core;\npub fn parse(s: &str) -> core::option::Option<(&str, &str)> { s.split_once('=') }";
    let body = range(source, "{ s.split_once('=') }");
    let rust_source_base_dir =
        eko::path::Path::new(root.to_str().expect("configured rust-src path is UTF-8"));
    let report = analyze_body_type_identity_with_loaded(
        "selected_char_pattern_chain",
        source,
        body,
        Some("2024"),
        Loaded {
            dependencies: &dependencies,
            rust_source_base_dir: Some(rust_source_base_dir),
            ..Loaded::default()
        },
    );
    let call = call_fact(&report, source, "s.split_once('=')");
    let callable = callable_for(call);
    assert_eq!(callable.kind, ResolvedCallableKind::SelectedMethod);
    let body_span = report.identity_facts.body_span.as_ref().unwrap();
    assert_eq!((body_span.start, body_span.end), (body.start, body.end));
    assert_eq!(call.span.file, body_span.file);
    assert_eq!(
        source.get(call.span.start as usize..call.span.end as usize),
        Some("s.split_once('=')")
    );

    let pattern_path = root.join("library/core/src/str/pattern.rs");
    let (pattern_path, pattern_source) = normalized_source(&pattern_path);
    let split_path = root.join("library/core/src/str/mod.rs");
    let (split_path, split_source) = normalized_source(&split_path);
    let split_source_attestation =
        known(&callable.source_attestation, "split_once source attestation");
    assert_eq!(callable.definition, split_source_attestation.definition);
    let context = split_source_attestation.source_context.as_ref().unwrap();
    assert_eq!(context.crate_name, "core");
    assert_eq!(context.stable_crate_id, callable.definition.stable_crate_id);
    assert_eq!(split_source_attestation.source_digest_scheme, "rustc_normalized_source_blake3_v1");
    let split_item = checked_source_slice(
        &split_path,
        &split_source_attestation.item_span,
        &split_source_attestation.source_content_digest,
        &split_source_attestation.item_digest,
        &split_source,
    );
    assert!(split_item.contains("pub fn split_once<P: Pattern>"));
    assert!(split_item.contains("delimiter.into_searcher(self).next_match()?"));

    let substitutions =
        known(callable.generic_substitutions.as_ref().unwrap(), "call substitutions");
    let char_substitution = substitutions.iter().find(|sub| matches!(
        &sub.argument,
        StaticFact::Known { value: ResolvedCallableGenericArgumentIdentity::Type { value } }
            if matches!(value.as_ref(), ResolvedCallableGenericTypeIdentity::Primitive { primitive: ResolvedPrimitive::Char })
    )).expect("compiler substitutes P with char");
    assert_eq!(
        char_substitution.parameter_kind,
        frontend::frontend_facts::ResolvedCallableGenericParameterKind::Type
    );

    let declared =
        known(callable.declared_trait_predicates.as_ref().unwrap(), "declared predicates");
    let obligations =
        known(callable.declared_trait_obligations.as_ref().unwrap(), "instantiated obligations");
    let pattern_obligation = obligations.iter().find(|obligation| {
        matches!((trait_ref(obligation), trait_source(obligation)), (Some(r), Some(a))
            if r.definition == a.definition
                && std::path::Path::new(a.item_span.file.as_ref()).ends_with("core/src/str/pattern.rs"))
    }).expect("source-attested Pattern obligation");
    let pattern_source_attestation = trait_source(pattern_obligation).unwrap();
    let pattern_item = checked_definition(
        &pattern_path,
        &pattern_source,
        pattern_source_attestation,
        DefinitionSourceKind::Trait,
    );
    assert!(pattern_item.contains("pub trait Pattern"));
    assert_core_context(&pattern_source_attestation.source_context, context);
    let instantiated_pattern = trait_ref(pattern_obligation).unwrap();
    assert_eq!(instantiated_pattern.arguments.len(), 1);
    assert!(matches!(
        known_type_argument(&instantiated_pattern.arguments[0]),
        Some(ResolvedCallableGenericTypeIdentity::Primitive { primitive: ResolvedPrimitive::Char })
    ));
    let declared_pattern = declared
        .iter()
        .find(|predicate| {
            matches!(
                &predicate.trait_ref,
                StaticFact::Known { value } if value.definition == instantiated_pattern.definition
            )
        })
        .expect("instantiated predicate maps to callable's declared Pattern predicate");
    let declared_pattern_ref = known(&declared_pattern.trait_ref, "declared Pattern ref");
    let Some(ResolvedCallableGenericTypeIdentity::Parameter { parameter }) =
        known_type_argument(&declared_pattern_ref.arguments[0])
    else {
        panic!("declared Pattern Self is not a compiler type parameter: {declared_pattern:?}");
    };
    assert_eq!(parameter, &char_substitution.parameter);

    let ResolvedCallableTraitSelection::UserDefined {
        implementation: pattern_impl,
        implemented_trait,
        implementation_source_attestation,
        associated_types,
        ..
    } = known(&pattern_obligation.selection, "Pattern selection")
    else {
        panic!("Pattern<char> was not resolved to a user-defined impl");
    };
    assert_eq!(known(implemented_trait, "implemented Pattern ref"), instantiated_pattern);
    let pattern_impl_source = known(implementation_source_attestation, "Pattern impl source");
    assert_eq!(&pattern_impl_source.definition, pattern_impl);
    let pattern_impl_item = checked_definition(
        &pattern_path,
        &pattern_source,
        pattern_impl_source,
        DefinitionSourceKind::TraitImplementation,
    );
    assert!(pattern_impl_item.contains("impl Pattern for char"));
    assert_core_context(&pattern_impl_source.source_context, context);

    let projections = known(associated_types, "Pattern associated types");
    let projection = projections.iter().find(|p| {
        matches!(&p.trait_item_source_attestation, StaticFact::Known { value: a }
            if a.definition == p.trait_associated_type
                && std::path::Path::new(a.item_span.file.as_ref()).ends_with("core/src/str/pattern.rs"))
    }).expect("compiler-mapped Pattern::Searcher projection");
    let assoc_source = known(&projection.trait_item_source_attestation, "Pattern::Searcher source");
    let assoc_item = checked_definition(
        &pattern_path,
        &pattern_source,
        assoc_source,
        DefinitionSourceKind::AssociatedType,
    );
    assert!(assoc_item.contains("type Searcher<'a>: Searcher<'a>"));
    assert_core_context(&assoc_source.source_context, context);
    let impl_assoc_id = known(&projection.implementation_item, "selected associated item");
    let impl_assoc_source = known(
        &projection.implementation_item_source_attestation,
        "selected associated item source",
    );
    assert_eq!(impl_assoc_id, &impl_assoc_source.definition);
    let impl_assoc_item = checked_definition(
        &pattern_path,
        &pattern_source,
        impl_assoc_source,
        DefinitionSourceKind::AssociatedType,
    );
    assert!(impl_assoc_item.contains("type Searcher<'a> = CharSearcher<'a>"));
    assert_core_context(&impl_assoc_source.source_context, context);

    let normalized_searcher = known(&projection.value, "normalized Searcher nominal");
    let ResolvedCallableGenericTypeIdentity::Adt {
        definition: char_searcher,
        source_attestation,
        arguments,
    } = normalized_searcher
    else {
        panic!("Searcher projection is not a resolved nominal ADT: {normalized_searcher:?}");
    };
    assert_eq!(char_searcher.stable_crate_id, pattern_impl.stable_crate_id);
    let nominal_source = known(
        source_attestation.as_ref().expect("nominal declaration attestation field"),
        "normalized Searcher nominal declaration source",
    );
    assert_eq!(&nominal_source.definition, char_searcher);
    let nominal_item = checked_definition(
        &pattern_path,
        &pattern_source,
        nominal_source,
        DefinitionSourceKind::NominalType,
    );
    assert!(nominal_item.contains("struct CharSearcher"));
    assert_core_context(&nominal_source.source_context, context);
    assert_eq!(arguments.len(), 1);
    assert!(matches!(
        &arguments[0],
        StaticFact::Known {
            value: ResolvedCallableGenericArgumentIdentity::Lifetime {
                value: ResolvedRegionIdentity::Erased
            }
        }
    ));
    let projection_args =
        known(&projection.projection_arguments, "full trait projection arguments");
    assert_eq!(projection_args.len(), 2, "parent Self plus the GAT lifetime");
    assert!(matches!(&projection_args[0].argument,
        StaticFact::Known { value: ResolvedCallableGenericArgumentIdentity::Type { value } }
            if matches!(value.as_ref(), ResolvedCallableGenericTypeIdentity::Primitive { primitive: ResolvedPrimitive::Char })));
    assert!(matches!(
        &projection_args[1].argument,
        StaticFact::Known {
            value: ResolvedCallableGenericArgumentIdentity::Lifetime {
                value: ResolvedRegionIdentity::Erased
            }
        }
    ));
    let impl_projection_args =
        known(&projection.implementation_item_arguments, "impl GAT arguments");
    assert_eq!(impl_projection_args.len(), 1);
    assert!(matches!(
        &impl_projection_args[0].argument,
        StaticFact::Known {
            value: ResolvedCallableGenericArgumentIdentity::Lifetime {
                value: ResolvedRegionIdentity::Erased
            }
        }
    ));

    let bounds = known(&projection.trait_obligations, "Searcher associated bounds");
    let searcher_bound = bounds.iter().find(|obligation| matches!(
        trait_source(obligation), Some(source)
            if std::path::Path::new(source.item_span.file.as_ref()).ends_with("core/src/str/pattern.rs")
                && source.definition_kind == DefinitionSourceKind::Trait
    )).expect("source-attested Searcher bound");
    let searcher_source = trait_source(searcher_bound).unwrap();
    let searcher_item = checked_definition(
        &pattern_path,
        &pattern_source,
        searcher_source,
        DefinitionSourceKind::Trait,
    );
    assert!(searcher_item.contains("pub unsafe trait Searcher"));
    assert_core_context(&searcher_source.source_context, context);
    let searcher_ref = trait_ref(searcher_bound).unwrap();
    assert_eq!(
        &searcher_source.definition, &searcher_ref.definition,
        "attested Searcher trait identity matches the selected trait ref"
    );
    assert_eq!(searcher_ref.arguments.len(), 2);
    assert_eq!(known_type_argument(&searcher_ref.arguments[0]), Some(normalized_searcher));
    assert!(matches!(
        &searcher_ref.arguments[1],
        StaticFact::Known {
            value: ResolvedCallableGenericArgumentIdentity::Lifetime {
                value: ResolvedRegionIdentity::Erased
            }
        }
    ));
    let ResolvedCallableTraitSelection::UserDefined {
        implementation: searcher_impl,
        implemented_trait,
        implementation_arguments,
        implementation_source_attestation,
        methods,
        ..
    } = known(&searcher_bound.selection, "Searcher<char> implementation")
    else {
        panic!("normalized associated type did not select a user-defined Searcher impl");
    };
    assert_eq!(known(implemented_trait, "implemented Searcher ref"), searcher_ref);
    let searcher_impl_source = known(implementation_source_attestation, "Searcher impl source");
    assert_eq!(&searcher_impl_source.definition, searcher_impl);
    let searcher_impl_item = checked_definition(
        &pattern_path,
        &pattern_source,
        searcher_impl_source,
        DefinitionSourceKind::TraitImplementation,
    );
    assert!(searcher_impl_item.contains("unsafe impl<'a> Searcher<'a> for CharSearcher<'a>"));
    assert_core_context(&searcher_impl_source.source_context, context);
    let impl_args = known(implementation_arguments, "selected Searcher impl arguments");
    assert_eq!(impl_args.len(), 1);
    assert!(matches!(
        &impl_args[0].argument,
        StaticFact::Known {
            value: ResolvedCallableGenericArgumentIdentity::Lifetime {
                value: ResolvedRegionIdentity::Erased
            }
        }
    ));

    let next_match_rows: Vec<_> = known(methods, "selected Searcher methods")
        .iter()
        .filter_map(|method| {
            let source = match &method.trait_method_source_attestation {
                StaticFact::Known { value }
                    if std::path::Path::new(value.item_span.file.as_ref())
                        .ends_with("core/src/str/pattern.rs") =>
                {
                    value
                }
                StaticFact::Known { .. } | StaticFact::Unknown { .. } => return None,
            };
            let item = checked_definition(
                &pattern_path,
                &pattern_source,
                source,
                DefinitionSourceKind::TraitMethod,
            );
            item.contains("fn next_match(").then_some((method, source))
        })
        .collect();
    assert_eq!(next_match_rows.len(), 1, "one exact Searcher::next_match item");
    let (method, method_source) = next_match_rows[0];
    assert_eq!(
        &method.trait_method, &method_source.definition,
        "attested next_match declaration matches the selected trait method identity"
    );
    assert_core_context(&method_source.source_context, context);
    let body = known(&method.selected_body, "selected next_match body");
    assert_eq!(&body.defining_owner, searcher_impl);
    let body_source = known(&body.source_attestation, "selected next_match body source");
    assert_eq!(body.definition, body_source.definition);
    let body_item = checked_definition(
        &pattern_path,
        &pattern_source,
        body_source,
        DefinitionSourceKind::ImplementationMethod,
    );
    assert!(body_item.contains("fn next_match("));
    assert!(body_item.contains("self.haystack.as_bytes().get(self.finger..self.finger_back)?"));
    assert_core_context(&body_source.source_context, context);
    let owner_source =
        known(&body.defining_owner_source_attestation, "selected Searcher owner source");
    assert_eq!(&owner_source.definition, searcher_impl);
    let owner_item = checked_definition(
        &pattern_path,
        &pattern_source,
        owner_source,
        DefinitionSourceKind::TraitImplementation,
    );
    assert!(owner_item.contains("unsafe impl<'a> Searcher<'a> for CharSearcher<'a>"));
    assert_core_context(&owner_source.source_context, context);
}

#[test]
#[ignore = "requires the exact pinned rust-src source configured by FRONTEND_RUST_SRC_ROOTS; the test writes core metadata with this frontend build"]
fn trait_proof_distinguishes_known_empty_from_generic_inference_and_indirect_unknowns() {
    ready();
    let (_scratch, _root, dependencies) = selected_trait_inputs("selected-trait-refusals");
    let loaded = Loaded { dependencies: &dependencies, ..Loaded::default() };

    let empty_source = "#![no_std]\nextern crate core;\nfn leaf() {} fn caller() { leaf(); }";
    let empty_report = analyze_body_type_identity_with_loaded(
        "known_empty_trait_facts",
        empty_source,
        range(empty_source, "{ leaf(); }"),
        Some("2024"),
        loaded,
    );
    let empty = callable_for(call_fact(&empty_report, empty_source, "leaf()"));
    assert!(
        matches!(empty.generic_substitutions.as_ref(), Some(StaticFact::Known { value }) if value.is_empty())
    );
    assert!(
        matches!(empty.declared_trait_predicates.as_ref(), Some(StaticFact::Known { value }) if value.is_empty())
    );
    assert!(
        matches!(empty.declared_trait_obligations.as_ref(), Some(StaticFact::Known { value }) if value.is_empty())
    );
    let wire = serde_json::to_value(empty).unwrap();
    assert_eq!(
        wire["declared_trait_obligations"]["known"]["value"].as_array().map(Vec::len),
        Some(0)
    );

    let generic_source = "#![no_std]\n#![feature(pattern)]\nextern crate core;\nfn generic<P: core::str::Pattern>(s: &str, p: P) -> core::option::Option<(&str, &str)> { s.split_once(p) }";
    let generic_report = analyze_body_type_identity_with_loaded(
        "generic_pattern_refusal",
        generic_source,
        range(generic_source, "{ s.split_once(p) }"),
        Some("2024"),
        loaded,
    );
    let generic = callable_for(call_fact(&generic_report, generic_source, "s.split_once(p)"));
    let substitutions =
        known(generic.generic_substitutions.as_ref().unwrap(), "generic substitutions");
    assert!(substitutions.iter().any(|sub| matches!(
        &sub.argument,
        StaticFact::Known { value: ResolvedCallableGenericArgumentIdentity::Type { value } }
            if matches!(value.as_ref(), ResolvedCallableGenericTypeIdentity::Parameter { .. })
    )));
    let obligations =
        known(generic.declared_trait_obligations.as_ref().unwrap(), "generic obligations");
    assert!(!obligations.is_empty());
    assert!(
        obligations
            .iter()
            .all(|obligation| matches!(&obligation.selection, StaticFact::Unknown { .. }))
    );
    assert!(
        !has_user_defined_selection(generic),
        "generic P cannot be assigned a concrete implementation"
    );

    let infer_source = "#![no_std]\nextern crate core;\nfn inferred(s: &str) { let _ = s.split_once(core::default::Default::default()); }";
    let infer_report = analyze_body_type_identity_with_loaded(
        "inference_pattern_refusal",
        infer_source,
        range(infer_source, "{ let _ = s.split_once(core::default::Default::default()); }"),
        Some("2024"),
        loaded,
    );
    assert_eq!(infer_report.type_facts.typeck_tainted_by_errors, Some(true));
    let infer_call =
        call_fact(&infer_report, infer_source, "s.split_once(core::default::Default::default())");
    match infer_call.resolved_callable.as_ref().expect("inference call status") {
        StaticFact::Known { value: Some(callable) } => {
            assert!(!has_user_defined_selection(callable));
            match callable.declared_trait_obligations.as_ref() {
                Some(StaticFact::Known { value }) => assert!(
                    !value.is_empty()
                        && value.iter().all(|o| matches!(&o.selection, StaticFact::Unknown { .. }))
                ),
                Some(StaticFact::Unknown { why }) => assert!(!why.is_empty()),
                None => panic!("new callable response omitted inferred obligation status"),
            }
        }
        StaticFact::Unknown { why } => assert!(!why.is_empty()),
        other => panic!("inference cannot be reported as a known non-call: {other:?}"),
    }

    let indirect_source = "#![no_std]\nextern crate core;\nfn indirect(call: fn()) { call(); }";
    let indirect_report = analyze_body_type_identity_with_loaded(
        "indirect_call_refusal",
        indirect_source,
        range(indirect_source, "{ call(); }"),
        Some("2024"),
        loaded,
    );
    let indirect = call_fact(&indirect_report, indirect_source, "call()");
    assert!(matches!(indirect.resolved_callable.as_ref(),
        Some(StaticFact::Unknown { why }) if why == "indirect_call_target"));
}

#[test]
fn injected_clone_function_item_identity_remains_unknown() {
    ready();
    let source = "struct Local; impl Clone for Local { fn clone(&self) -> Self { Local } } fn body(value: &Local) -> Local { <Local as Clone>::clone(value) }";
    let body = range(source, "{ <Local as Clone>::clone(value) }");
    let report = analyze_body_type_identity("body_identity_injected_clone", source, body);
    assert!(!report.type_facts.fatal, "{:?}", report.type_facts);

    let item = identity_expression_for_range(&report, source, "<Local as Clone>::clone");
    assert!(
        matches!(
            &item.resolved_ty,
            StaticFact::Unknown { why } if why == "synthetic_declaration_context"
        ),
        "the injected Clone trait method's DefId must not be promoted to a known item identity: {item:?}"
    );

    let call = identity_expression_for_range(&report, source, "<Local as Clone>::clone(value)");
    assert!(
        matches!(
            &call.resolved_callable,
            Some(StaticFact::Unknown { why }) if why == "synthetic_declaration_context"
        ),
        "call identity already refuses the synthetic declaration: {call:?}"
    );
}
