use frontend::frontend_facts::analyze_source;
use frontend::rustc_session::config::Options;

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

#[test]
fn no_sysroot_analysis_uses_only_the_supplied_source() {
    frontend::unwind_janky::install_catcher(catcher);
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    let options = Options::default();
    assert!(options.sysroot.path().is_none());
    assert!(options.sysroot.all_paths().next().is_none());

    let facts = analyze_source("no_sysroot_search", "#![no_core]\npub struct Parsed;")
        .expect("the no_core source parses without a sysroot");
    assert!(facts.definitions.iter().any(|definition| definition.name == "Parsed"));
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
}
