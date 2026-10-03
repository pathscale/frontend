use frontend::frontend_facts::effects::{
    self, EffectEvidence, PanicKind, ResolvedMacro, ResolvedMethod, StaticFact, TextRange,
};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn install_catcher() {
    frontend::unwind_janky::install_catcher(catcher);
}

#[test]
fn resolved_todo_body_diverges() {
    install_catcher();
    let source = "fn pending() { todo!() }";
    let start = source.find("todo!()").expect("macro invocation") as u32;
    let evidence = EffectEvidence {
        macros: vec![ResolvedMacro {
            span: TextRange { start, end: start + "todo!()".len() as u32 },
            def_path: "core::macros::todo".to_string(),
        }],
        ..EffectEvidence::default()
    };

    let before = frontend::rustc_const_eval::interp_cx_new_count();
    let found = effects::analyze_source_with_evidence(source, &evidence).expect("source parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].diverges, StaticFact::Known { value: true });
}

#[test]
fn scan_without_panic_sites_is_empty() {
    install_catcher();
    let source = "fn clean() { let count = 2; let _ = count; }";

    let before = frontend::rustc_const_eval::interp_cx_new_count();
    let found = effects::analyze_source(source).expect("source parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
    assert_eq!(found.len(), 1);
    assert!(found[0].panic_sites.is_empty());
}

#[test]
fn option_unwrap_is_reported_from_its_resolved_method() {
    install_catcher();
    let source = "fn value(input: Option<u8>) -> u8 { input.unwrap() }";
    let start = source.find("input.unwrap()").expect("method call") as u32;
    let evidence = EffectEvidence {
        methods: vec![ResolvedMethod {
            span: TextRange { start, end: start + "input.unwrap()".len() as u32 },
            def_path: "core::option::Option::unwrap".to_string(),
        }],
        ..EffectEvidence::default()
    };

    let before = frontend::rustc_const_eval::interp_cx_new_count();
    let found = effects::analyze_source_with_evidence(source, &evidence).expect("source parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].panic_sites.len(), 1);
    assert_eq!(found[0].panic_sites[0].kind, PanicKind::OptionUnwrap);
}

#[test]
fn panics_doc_heading_is_read_from_the_item_attributes() {
    install_catcher();
    let source = "/// # Panics\n/// This function can panic.\nfn documented() {}";

    let before = frontend::rustc_const_eval::interp_cx_new_count();
    let found = effects::analyze_source(source).expect("source parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
    assert_eq!(found.len(), 1);
    assert!(found[0].doc_sections.panics);
    assert!(!found[0].doc_sections.errors);
    assert!(!found[0].doc_sections.safety);
}
