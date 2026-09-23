// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::num::NonZero;
use eko::env;
use eko::path::Path;
use eko::thread::OnceLock;

use crate::frontend_semantics::TargetConfig;
use crate::frontend_semantics::target_features::internal_target_features;
use crate::rustc_ast as ast;
use crate::rustc_attr_parsing::ShouldEmit;
use crate::rustc_data_structures::base_n::{CASE_INSENSITIVE, ToBaseN};
use crate::rustc_data_structures::sync;
use crate::rustc_middle::ty::CurrentGcx;
use crate::rustc_session::config::{Cfg, Jobs, OutFileName, OutputFilenames, OutputTypes};
use crate::rustc_session::{EarlyDiagCtxt, Session};
use crate::rustc_span::edition::Edition;
use crate::rustc_span::source_map::SourceMapInputs;
use crate::rustc_span::{SessionGlobals, Symbol, sym};
use crate::rustc_target::spec::Target;

use crate::rustc_interface::diagnostics;
use crate::rustc_interface::passes::parse_crate_name;

/// Adds `target_feature = "..."` cfgs for a variety of platform
/// specific features (SSE, NEON etc.).
///
/// This is performed by checking whether a set of permitted features
/// is available on the target machine, by querying the `TargetConfig` from the codegen backend.
pub(crate) fn add_configuration(
    cfg: &mut Cfg,
    target_config: &TargetConfig,
    target: &Target,
    is_nightly_build: bool,
    is_crt_static: bool,
) {
    // Add some of the target features to `cfg`.
    cfg.extend(
        target
            .rust_target_features()
            .iter()
            .filter_map(|(feature, gate, _)| {
                if gate.in_cfg()
                    && (is_nightly_build || gate.requires_nightly(/* in_cfg */ true).is_none())
                {
                    Some(Symbol::intern(feature))
                } else {
                    None
                }
            })
            .filter(|feature| target_config.internal_target_features.contains(&feature))
            .map(|feature| (sym::target_feature, Some(feature))),
    );

    if target_config.has_reliable_f16 {
        cfg.insert((sym::target_has_reliable_f16, None));
    }
    if target_config.has_reliable_f16_math {
        cfg.insert((sym::target_has_reliable_f16_math, None));
    }
    if target_config.has_reliable_f128 {
        cfg.insert((sym::target_has_reliable_f128, None));
    }
    if target_config.has_reliable_f128_math {
        cfg.insert((sym::target_has_reliable_f128_math, None));
    }

    if is_crt_static {
        cfg.insert((sym::target_feature, Some(sym::crt_dash_static)));
    }
}

/// Ensures that all target features required by the ABI are present.
/// Must be called after `internal_target_features` has been populated!
pub(crate) fn check_abi_required_features(sess: &Session) {
    let abi_feature_constraints = sess.target.abi_required_features();
    // We check this against `internal_target_features` as that is conveniently already
    // back-translated to rustc feature names, taking into account `-Ctarget-cpu` and `-Ctarget-feature`.
    // Just double-check that the features we care about are actually on our list.
    for feature in
        abi_feature_constraints.required.iter().chain(abi_feature_constraints.incompatible.iter())
    {
        assert!(
            sess.target.rust_target_features().iter().any(|(name, ..)| feature == name),
            "target feature {feature} is required/incompatible for the current ABI but not a recognized feature for this target"
        );
    }

    for feature in abi_feature_constraints.required {
        if !sess.internal_target_features.contains(&Symbol::intern(feature)) {
            sess.dcx()
                .emit_warn(diagnostics::AbiRequiredTargetFeature { feature, enabled: "enabled" });
        }
    }
    for feature in abi_feature_constraints.incompatible {
        if sess.internal_target_features.contains(&Symbol::intern(feature)) {
            sess.dcx()
                .emit_warn(diagnostics::AbiRequiredTargetFeature { feature, enabled: "disabled" });
        }
    }
}

pub static STACK_SIZE: OnceLock<usize> = OnceLock::new();
pub const DEFAULT_STACK_SIZE: usize = 16 * 1024 * 1024;

fn init_stack_size(early_dcx: &EarlyDiagCtxt) -> usize {
    // Obey the environment setting or default
    *STACK_SIZE.get_or_init(|| {
        env::var_os("RUST_MIN_STACK")
            .as_ref()
            // `eko::env::var_os` hands back the raw bytes rather than an `OsString`,
            // so the lossy decode happens here instead of on an `OsStr`.
            .map(|bytes| String::from_utf8_lossy(bytes))
            // if someone finds out `export RUST_MIN_STACK=640000` isn't enough stack
            // they might try to "unset" it by running `RUST_MIN_STACK=  rustc code.rs`
            // this is wrong, but std would nonetheless "do what they mean", so let's do likewise
            .filter(|s| !s.trim().is_empty())
            // rustc is a batch program, so error early on inputs which are unlikely to be intended
            // so no one thinks we parsed them setting `RUST_MIN_STACK="64 megabytes"`
            // FIXME: we could accept `RUST_MIN_STACK=64MB`, perhaps?
            .map(|s| {
                let s = s.trim();
                s.parse::<usize>().unwrap_or_else(|_| {
                    let mut err = early_dcx.early_struct_fatal(format!(
                        r#"`RUST_MIN_STACK` should be a number of bytes, but was "{s}""#,
                    ));
                    err.note("you can also unset `RUST_MIN_STACK` to use the default stack size");
                    err.emit()
                })
            })
            // otherwise pick a consistent default
            .unwrap_or(DEFAULT_STACK_SIZE)
    })
}

fn run_on_current_thread_with_globals<F: FnOnce(CurrentGcx) -> R + Send, R: Send>(
    edition: Edition,
    sm_inputs: SourceMapInputs,
    extra_symbols: &[&'static str],
    f: F,
) -> R {
    // The caller supplies the thread and its stack size. Spawning a
    // second request-scoped thread here was a batch-driver lifecycle hidden under the server.
    crate::rustc_span::create_session_globals_then(edition, extra_symbols, Some(sm_inputs), || {
        f(CurrentGcx::new())
    })
}

/// How many threads a session runs on: `jobs.frontend` when it is two or more and the
/// `parallel` feature is built, else one, which is the serial compiler.
///
/// One asked-for thread is serial, not "parallel on one worker": a width of one has no helper to
/// hand anything to, and running the thread-safe locks and the query system's latch waits with
/// nobody to wait for would only cost.
pub fn session_width(jobs: Jobs) -> usize {
    match jobs.frontend {
        Some(threads) if cfg!(feature = "parallel") && threads.get() > 1 => threads.get(),
        _ => 1,
    }
}

pub(crate) fn run_in_thread_pool_with_globals<F: FnOnce(CurrentGcx) -> R + Send, R: Send>(
    thread_builder_diag: &EarlyDiagCtxt,
    edition: Edition,
    jobs: Jobs,
    extra_symbols: &[&'static str],
    sm_inputs: SourceMapInputs,
    f: F,
) -> R {
    // Still read, because it still validates: a malformed `RUST_MIN_STACK` is refused here, as
    // before. This crate starts no thread to give the size to. nagoya's shared pool starts its
    // threads through `std`, which reads the same variable, so setting it is how that pool gets
    // compiler-sized stacks today; see `NAGOYA-STACK-PATCH.md`.
    let _ = init_stack_size(thread_builder_diag);

    let width = NonZero::new(session_width(jobs)).expect("a session runs on at least one thread");

    // **The session's thread, plus nagoya's pool when the session is parallel.**
    //
    // This had two paths: a `rustc_thread_pool` (rayon-core) pool of `jobs.frontend` workers that
    // this function built, with a deadlock handler, and a single-thread one. The rustc-owned pool
    // is gone for good. What replaced it builds no pool at all: the session runs on the caller's
    // thread, and when it is parallel each stage (`sync::stages`) hands items to helpers on
    // nagoya's pool, which nagoya owns and which serves every session in the process.
    //
    // The deadlock handler went with the pool, and nothing here replaces it. It existed because
    // rayon workers blocked on each other's queries with nothing to detect a cycle; the query
    // system now detects a cycle at the moment a wait would close one (`QueryWaitGraph`), and a
    // stage's waits only ever wait for running items (see `stage.rs`).
    run_on_current_thread_with_globals(
        edition,
        sm_inputs,
        extra_symbols,
        |current_gcx| {
            // One registry per session, with one slot per thread the session may use. This
            // thread holds slot 0 for the whole session; pool helpers lease the others while they
            // run this session's items. Every `WorkerLocal` the session creates is sized by it.
            //
            // It replaced a registry kept on the thread across requests, sized by whichever
            // session came first: a cache of the first request's shape, and wrong once sessions
            // with different widths share threads.
            let registry = sync::Registry::new(width);
            let slot = registry.lease().expect("a new registry has a free slot");
            let _in_registry = slot.enter();

            f(current_gcx)
        },
    )
}

// ---- what a parallel item is given ---------------------------------------------------------

/// Register what the parallel stages install around every item: this session's
/// `SessionGlobals` and `ImplicitCtxt`, and the diagnostics collection that forwards each item's
/// diagnostics in item order. `run_compiler` calls it; registering again is a no-op, so a caller
/// that runs stages outside `run_compiler` can call it too.
///
/// `rustc_data_structures` cannot name any of the three, so it takes them as hooks (see
/// `sync::ContextHook` and `sync::ItemHook`). They are used only by parallel sessions.
pub fn install_parallel_context() {
    sync::install_context_hook(&SESSION_GLOBALS_HOOK);
    sync::install_context_hook(&IMPLICIT_CTXT_HOOK);
    sync::install_item_hook(&DIAGNOSTICS_HOOK);
}

static SESSION_GLOBALS_HOOK: sync::ContextHook =
    sync::ContextHook { capture: capture_session_globals, enter: enter_session_globals };

fn capture_session_globals() -> *const () {
    if crate::rustc_span::session_globals_are_set() {
        crate::rustc_span::with_session_globals(|globals| core::ptr::from_ref(globals).cast())
    } else {
        core::ptr::null()
    }
}

/// Install the captured `SessionGlobals` around `run`, unless this thread already has them.
///
/// # Safety
///
/// `captured` came from `capture_session_globals` on a thread that has not left the stage scope
/// it was captured for; see `sync::ContextHook`.
unsafe fn enter_session_globals(captured: *const (), run: &mut dyn FnMut()) {
    if captured.is_null() {
        return run();
    }
    // SAFETY: the caller's contract: the `SessionGlobals` outlive this call.
    let globals = unsafe { &*captured.cast::<SessionGlobals>() };
    if crate::rustc_span::session_globals_are_set() {
        // The session's own thread, or a pool thread running this item while it waits inside
        // another item of the same session. Anything else would mean two sessions on one thread.
        let same = crate::rustc_span::with_session_globals(|current| core::ptr::eq(current, globals));
        assert!(same, "a parallel item ran on a thread that is inside another session");
        run()
    } else {
        crate::rustc_span::set_session_globals_then(globals, run)
    }
}

static IMPLICIT_CTXT_HOOK: sync::ContextHook =
    sync::ContextHook { capture: capture_implicit_ctxt, enter: enter_implicit_ctxt };

fn capture_implicit_ctxt() -> *const () {
    crate::rustc_middle::ty::tls::with_context_opt(|icx| match icx {
        Some(icx) => core::ptr::from_ref(icx).cast(),
        None => core::ptr::null(),
    })
}

/// Install the captured `ImplicitCtxt` around `run`.
///
/// It is installed even on a thread that already has one: the item's queries must see the
/// context the stage was started in, whose `query` is the parent the query system's cycle check
/// walks (`QueryWaitGraph`), not whatever context a waiting thread happens to be inside.
///
/// # Safety
///
/// As `enter_session_globals`.
unsafe fn enter_implicit_ctxt(captured: *const (), run: &mut dyn FnMut()) {
    use crate::rustc_middle::ty::tls;
    if captured.is_null() {
        return run();
    }
    // SAFETY: the caller's contract: the `ImplicitCtxt` outlives this call. The lifetimes are the
    // captured context's own, erased; nothing here outlives it.
    let icx = unsafe { &*captured.cast::<tls::ImplicitCtxt<'static, 'static>>() };
    tls::enter_context(icx, run)
}

static DIAGNOSTICS_HOOK: sync::ItemHook = sync::ItemHook {
    begin: begin_ordered_replay,
    run: run_collecting_item,
    finish: finish_ordered_replay,
    discard: discard_ordered_replay,
};

/// One `OrderedReplay` per stage, created on the thread starting the stage (it captures that
/// thread's enclosing item, if any, as where to forward).
fn begin_ordered_replay() -> *mut () {
    Box::into_raw(Box::new(crate::rustc_errors::OrderedReplay::new())).cast()
}

/// # Safety
///
/// `state` came from `begin_ordered_replay` and has not been finished or discarded.
unsafe fn run_collecting_item(state: *const (), index: usize, item: &mut dyn FnMut()) -> bool {
    // SAFETY: the caller's contract.
    let replay = unsafe { &*state.cast::<crate::rustc_errors::OrderedReplay>() };
    // Through the replay, not a free function: the item's scope carries the replay's root, the
    // oldest stage of its chain, which is what a query run inside the item may capture.
    let run = replay.collect(item);
    let fatal = run.result.is_err();
    replay.ready(index, run.diagnostics, fatal);
    fatal
}

/// # Safety
///
/// As `run_collecting_item`, and this is its last use.
unsafe fn finish_ordered_replay(state: *mut ()) {
    // SAFETY: the caller's contract.
    let replay = unsafe { Box::from_raw(state.cast::<crate::rustc_errors::OrderedReplay>()) };
    if let Err(fatal) = replay.finish() {
        fatal.raise()
    }
}

/// # Safety
///
/// As `finish_ordered_replay`.
unsafe fn discard_ordered_replay(state: *mut ()) {
    // SAFETY: the caller's contract.
    drop(unsafe { Box::from_raw(state.cast::<crate::rustc_errors::OrderedReplay>()) });
}


/// Target facts the Rust frontend needs before it can build `cfg(target_feature)`.
///
/// These are semantic input to parsing and analysis, so they are read from the target rather than
/// asked of a codegen backend. Upstream routes the question through the backend because a backend
/// is what knows which features it can actually emit; with no backend in the tree the target spec
/// is the only source there is, and it is the right one for `cfg(target_feature)`, which is a
/// question about the target and not about who generates code for it.
pub(crate) fn frontend_target_config(sess: &Session) -> TargetConfig {
    let abi_required_features = sess.target.abi_required_features();
    let internal_target_features = internal_target_features::<0>(
        sess,
        |_feature| Default::default(),
        |feature| abi_required_features.required.contains(&feature),
    );

    TargetConfig {
        internal_target_features,
        has_reliable_f16: true,
        has_reliable_f16_math: true,
        has_reliable_f128: true,
        has_reliable_f128_math: true,
    }
}

fn multiple_output_types_to_stdout(
    output_types: &OutputTypes,
    single_output_file_is_stdout: bool,
) -> bool {
    // No `IsTerminal` import: `is_terminal` is an inherent method on `eko`'s `Stream`.
    if eko::file::stdout().is_terminal() {
        // If stdout is a tty, check if multiple text output types are
        // specified by `--emit foo=- --emit bar=-` or `-o - --emit foo,bar`
        let named_text_types = output_types
            .iter()
            .filter(|(f, o)| f.is_text_output() && *o == &Some(OutFileName::Stdout))
            .count();
        let unnamed_text_types =
            output_types.iter().filter(|(f, o)| f.is_text_output() && o.is_none()).count();
        named_text_types > 1 || unnamed_text_types > 1 && single_output_file_is_stdout
    } else {
        // Otherwise, all the output types should be checked
        let named_types =
            output_types.values().filter(|o| *o == &Some(OutFileName::Stdout)).count();
        let unnamed_types = output_types.values().filter(|o| o.is_none()).count();
        named_types > 1 || unnamed_types > 1 && single_output_file_is_stdout
    }
}

pub fn build_output_filenames(attrs: &[ast::Attribute], sess: &Session) -> OutputFilenames {
    if multiple_output_types_to_stdout(
        &sess.opts.output_types,
        sess.io.output_file == Some(OutFileName::Stdout),
    ) {
        sess.dcx().emit_fatal(diagnostics::MultipleOutputTypesToStdout);
    }

    let crate_name =
        sess.opts.crate_name.clone().or_else(|| {
            parse_crate_name(sess, attrs, ShouldEmit::Nothing).map(|i| i.0.to_string())
        });

    let invocation_temp = sess
        .opts
        .incremental
        .as_ref()
        .map(|_| invocation_suffix().to_base_fixed_len(CASE_INSENSITIVE).to_string());

    match sess.io.output_file {
        None => {
            // "-" as input file will cause the parser to read from stdin so we
            // have to make up a name
            // We want to toss everything after the final '.'
            let dirpath = sess.io.output_dir.clone().unwrap_or_default();

            // If a crate name is present, we use it as the link name
            let stem = crate_name.clone().unwrap_or_else(|| sess.io.input.filestem().to_owned());

            OutputFilenames::new(
                dirpath,
                crate_name.unwrap_or_else(|| stem.replace('-', "_")),
                stem,
                None,
                sess.io.temps_dir.clone(),
                invocation_temp,
                sess.opts.unstable_opts.split_dwarf_out_dir.clone(),
                sess.opts.cg.extra_filename.clone(),
                sess.opts.output_types.clone(),
            )
        }

        Some(ref out_file) => {
            let unnamed_output_types =
                sess.opts.output_types.values().filter(|a| a.is_none()).count();
            let ofile = if unnamed_output_types > 1 {
                sess.dcx().emit_warn(diagnostics::MultipleOutputTypesAdaption);
                None
            } else {
                if !sess.opts.cg.extra_filename.is_empty() {
                    sess.dcx().emit_warn(diagnostics::IgnoringExtraFilename);
                }
                Some(out_file.clone())
            };
            if sess.io.output_dir.is_some() {
                sess.dcx().emit_warn(diagnostics::IgnoringOutDir);
            }

            let out_filestem =
                out_file.filestem().unwrap_or_default().to_str().unwrap().to_string();
            OutputFilenames::new(
                out_file.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
                crate_name.unwrap_or_else(|| out_filestem.replace('-', "_")),
                out_filestem,
                ofile,
                sess.io.temps_dir.clone(),
                invocation_temp,
                sess.opts.unstable_opts.split_dwarf_out_dir.clone(),
                sess.opts.cg.extra_filename.clone(),
                sess.opts.output_types.clone(),
            )
        }
    }
}

/// Returns a version string such as "1.46.0 (04488afe3 2020-08-24)" when invoked by an in-tree tool.
// Was a `pub macro` (decl_macro). `#[macro_export]` plus the `pub use` below keeps the
// `rustc_interface::util::version_str!` path working. The expansion reads `CFG_VERSION` from the
// environment of the crate that invokes it, as before.
#[macro_export]
macro_rules! version_str {
    () => {
        ::core::option_env!("CFG_VERSION")
    };
}
pub use crate::version_str;

/// Returns the version string for `rustc` itself (which may be different from a tool version).
pub fn rustc_version_str() -> Option<&'static str> {
    version_str!()
}

/// The version claimed when `CFG_VERSION` is not set at build time.
///
/// This is what `scripts/build-sysroot.sh` produces, because bootstrap stamps `1.100.0-dev` on a
/// locally built compiler. A consumer that adds these crates as plain dependencies sets no
/// environment variables, so without a default it would claim `"unknown"` and fail against every
/// sysroot in existence, with an `E0514` naming a version nobody chose.
///
/// Override per session with [`Config::rustc_version`](crate::rustc_interface::interface::Config), which
/// [`rustc_version_of_sysroot`] fills in from whichever sysroot you actually point at.
///
/// Keep in step with `CFG_VERSION` in `.cargo/config.toml` and `DEFAULT_RELEASE` in
/// `rustc_macros`.
pub const DEFAULT_CFG_VERSION: &str = "1.100.0-dev";

/// The version string a sysroot expects, read from the sysroot itself.
///
/// # When you need this
///
/// Crate metadata records the version string of the compiler that wrote it, and loading an rlib
/// whose string differs is `E0514`. `CFG_VERSION` is the compiled-in default and is the right
/// answer whenever you use the sysroot this crate was built against, which is the common case.
///
/// Use this when you point at a *different* sysroot. The choice is made when you construct a
/// [`Config`](crate::rustc_interface::interface::Config), so the override belongs there too, and asking the sysroot
/// beats writing the string down twice. Getting it wrong is not a version complaint: an older
/// sysroot desyncs inside a serializer and a newer one fails in type checking, and neither says
/// which knob was wrong.
///
/// ```ignore
/// config.rustc_version = rustc_version_of_sysroot(&sysroot);
/// ```
///
/// `None` means the sysroot has no `bin/rustc` to ask - a bare library tree - and the compiled-in
/// `CFG_VERSION` stands. That is a real case and not an error.
///
/// The leading `rustc ` is removed, because what gets compared is `rustc ` *plus* this value, and
/// keeping the prefix yields `rustc rustc 1.100.0-dev`: wrong in a way that looks right in every
/// message that prints it.
pub fn rustc_version_of_sysroot(sysroot: &eko::path::Path) -> Option<alloc::string::String> {
    use alloc::string::{String, ToString};

    let rustc = sysroot.join("bin").join("rustc");
    let out = eko::command::Command::new(&rustc).arg("--version").output()?;
    if !out.success() {
        return None;
    }
    let line = String::from_utf8(out.stdout).ok()?;
    let line = line.trim();
    let version = line.strip_prefix("rustc ").unwrap_or(line).trim();
    (!version.is_empty()).then(|| version.to_string())
}

/// A suffix that distinguishes one invocation's temporary files from another's.
///
/// This was `rand::rng().next_u32()`, which is `thread_rng` and therefore `std`. The requirement
/// here is uniqueness, not unpredictability: the value names a scratch file inside the caller's
/// own incremental directory, and nothing trusts it. A process id, the monotonic clock and a
/// counter give that without a random number generator - two invocations differ by pid, two
/// files within one invocation differ by the counter, and a pid reused after a reboot differs by
/// the clock.
fn invocation_suffix() -> u32 {
    use core::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let pid = eko::proc::id();
    let now = eko::time::Instant::now().elapsed().subsec_nanos();
    pid ^ now.rotate_left(11) ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(22)
}
