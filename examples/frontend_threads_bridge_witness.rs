//! A linked witness for the caller-thread proc-macro bridge.
//! Receives Rust token text on stdin and prints its unchanged expansion.
//! No test harness, worker, timer or message channel is involved.

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use frontend::rustc_ast::tokenstream::TokenStream as ServerTokens;
use frontend::rustc_ast_pretty::pprust;
use frontend::rustc_data_structures::sync::WorkerLocal;
use frontend::rustc_errors::emitter::Emitter;
use frontend::rustc_errors::{DiagInner, ErrorGuaranteed};
use frontend::rustc_expand::base::{BangProcMacro as _, ExtCtxt};
use frontend::rustc_expand::expand::ExpansionConfig;
use frontend::rustc_expand::proc_macro::BangProcMacro;
use frontend::rustc_feature::UnstableFeatures;
use frontend::rustc_interface::{Config, create_and_enter_global_ctxt, parse, run_compiler};
use frontend::rustc_middle::ty::tls;
use frontend::rustc_proc_macro as pm;
use frontend::rustc_proc_macro::bridge::client::Client;
use frontend::rustc_resolve::Resolver;
use frontend::rustc_session::config::{Input, Options, ProcMacroExecutionStrategy};
use frontend::rustc_span::def_id::{CRATE_DEF_ID, LOCAL_CRATE};
use frontend::rustc_span::hygiene::{ExpnData, ExpnKind, LocalExpnId, MacroKind};
use frontend::rustc_span::source_map::SourceMap;
use frontend::rustc_span::{DUMMY_SP, FileName, Symbol};

static USING_INTERNAL_FEATURES: AtomicBool = AtomicBool::new(false);

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

struct Messages(Arc<eko::thread::Mutex<Vec<String>>>);

impl Emitter for Messages {
    fn emit_diagnostic(&mut self, diag: DiagInner) {
        // Include child help diagnostics, where the bridge's PanicMessage is rendered.
        self.0.lock().push(format!("{diag:?}"));
    }

    fn source_map(&self) -> Option<&SourceMap> {
        None
    }
}

// Resolver arenas borrow themselves. Keep the same destruction order as
// create_and_enter_global_ctxt: the context first, its arena owner last.
struct FreeOnDrop<T>(*mut T);

impl<T> Drop for FreeOnDrop<T> {
    fn drop(&mut self) {
        // SAFETY: each pointer comes from Box::into_raw below; no arena borrow
        // escapes expand, and the resolver has already been destroyed here.
        drop(unsafe { Box::from_raw(self.0) });
    }
}

fn expand(client: Client, input: ServerTokens) -> Result<ServerTokens, ErrorGuaranteed> {
    tls::with(|tcx| {
        let owner = FreeOnDrop(Box::into_raw(Box::new(WorkerLocal::new(|_| Resolver::arenas()))));
        // SAFETY: owner outlives resolver and ecx, and the output owns its token data.
        let arenas = unsafe { &*owner.0 };
        let mut resolver = Resolver::new(tcx, &[], DUMMY_SP, DUMMY_SP, arenas);
        let config = ExpansionConfig::default(tcx.crate_name(LOCAL_CRATE), tcx.features());
        let mut ecx = ExtCtxt::new(tcx.sess, config, &mut resolver, None);
        ecx.current_expansion.id = tcx.with_stable_hashing_context(|hcx| LocalExpnId::fresh(
            ExpnData::default(
                ExpnKind::Macro(MacroKind::Bang, Symbol::intern("bridge_witness")),
                DUMMY_SP,
                tcx.sess.edition(),
                Some(CRATE_DEF_ID.to_def_id()),
                None,
            ),
            hcx,
        ));
        BangProcMacro { client }.expand(&mut ecx, DUMMY_SP, input)
    })
}

fn echo(input: pm::TokenStream) -> pm::TokenStream {
    input
}

fn panic_client(_: pm::TokenStream) -> pm::TokenStream {
    panic!("frontend_threads_client_panic")
}

fn outer(input: pm::TokenStream) -> pm::TokenStream {
    let held = pm::Ident::new("outer_symbol_survives", pm::Span::call_site());
    let refused = expand(Client::expand1(echo), ServerTokens::default());
    assert!(refused.is_err(), "frontend_threads_nested_refusal");
    assert_eq!(held.to_string(), "outer_symbol_survives", "frontend_threads_outer_symbol");
    assert!(pm::is_available(), "frontend_threads_outer_bridge");
    input
}

fn run(source: Arc<String>, legacy: bool) {
    let messages = Arc::new(eko::thread::Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&messages);
    let mut opts = Options::default();
    opts.edition = frontend::rustc_span::edition::Edition::Edition2024;
    opts.unstable_features = UnstableFeatures::Allow;
    if legacy {
        opts.unstable_opts.proc_macro_execution_strategy = ProcMacroExecutionStrategy::CrossThread;
    }
    let config = Config {
        opts,
        input: Input::Str {
            name: FileName::Custom("bridge_witness_crate.rs".into()),
            input: Arc::new("#![feature(no_core)]
#![no_core]
".to_string()),
        },
        psess_created: Some(Box::new(move |psess| psess.set_emitter(Box::new(Messages(sink))))),
        using_internal_features: &USING_INTERNAL_FEATURES,
        crate_cfg: Vec::new(),
    };
    run_compiler(config, |compiler| {
        let krate = parse(&compiler.sess);
        create_and_enter_global_ctxt(compiler, krate, |tcx| {
            let input = frontend::rustc_parse::source_str_to_stream(
                &tcx.sess.psess,
                FileName::Custom("bridge_witness_input.rs".into()),
                source,
                None,
            ).unwrap_or_else(|_| panic!("frontend_threads_input_tokens"));
            let before = pprust::tts_to_string(&input);
            if legacy {
                assert!(expand(Client::expand1(echo), input).is_err(), "frontend_threads_legacy_refusal");
                let recorded = messages.lock();
                assert!(recorded.iter().any(|m| m.contains("frontend_proc_macro_cross_thread_removed")),
                    "frontend_threads_legacy_error_name: {:?}", &*recorded);
                println!("frontend_threads_legacy_refusal: ok");
            } else {
                let output = expand(Client::expand1(outer), input.clone())
                    .unwrap_or_else(|_| panic!("frontend_threads_outer_result"));
                assert_eq!(pprust::tts_to_string(&output), before, "frontend_threads_echo_tokens");
                assert!(!pm::is_available(), "frontend_threads_bridge_restored");
                assert!(expand(Client::expand1(panic_client), input.clone()).is_err(),
                    "frontend_threads_panic_reported");
                let recovered = expand(Client::expand1(echo), input)
                    .unwrap_or_else(|_| panic!("frontend_threads_after_panic"));
                assert_eq!(pprust::tts_to_string(&recovered), before, "frontend_threads_recovered_tokens");
                let recorded = messages.lock();
                assert!(recorded.iter().any(|m| m.contains("frontend_proc_macro_nested_bridge")),
                    "frontend_threads_nested_error_name: {:?}", &*recorded);
                assert!(recorded.iter().any(|m| m.contains("frontend_threads_client_panic")),
                    "frontend_threads_panic_error_name: {:?}", &*recorded);
                println!("frontend_threads_bridge: {before}");
            }
            // The refusal and deliberate panic are the witness's expected diagnostics.
            tcx.dcx().reset_err_count();
        });
    });
}

fn main() {
    frontend::unwind_janky::install_catcher(catcher);
    let mut source = String::new();
    io::stdin().read_to_string(&mut source).expect("frontend_threads_stdin");
    assert!(!source.trim().is_empty(), "frontend_threads_nonempty_input");
    run(Arc::new(source), std::env::args().any(|arg| arg == "--legacy"));
}
