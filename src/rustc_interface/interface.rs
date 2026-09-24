// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::sync::Arc;
use core::result;

use crate::rustc_ast::{LitKind, MetaItemKind, token};
use crate::rustc_data_structures::fx::FxHashSet;
use crate::rustc_errors::{DiagCtxtHandle, ErrorGuaranteed};
use crate::rustc_middle::ty;
use crate::rustc_middle::ty::CurrentGcx;
use crate::rustc_parse::lexer::StripTokens;
use crate::rustc_parse::new_parser_from_source_str;
use crate::rustc_parse::parser::Recovery;
use crate::rustc_query_impl::print_query_stack;
use crate::rustc_session::config::{self, Cfg, CheckCfg, ExpectedValues, Input};
use crate::rustc_session::parse::ParseSess;
use crate::rustc_session::{CompilerIO, EarlyDiagCtxt, Session};
use crate::rustc_span::source_map::{RealFileLoader, SourceMapInputs};
use crate::rustc_span::{FileName, sym};
use tracing::trace;

use crate::rustc_interface::util;

pub type Result<T> = result::Result<T, ErrorGuaranteed>;

/// Represents a compiler session. Note that every `Compiler` contains a
/// `Session`, but `Compiler` also contains some things that cannot be in
/// `Session`, due to `Session` being in a crate that has many fewer
/// dependencies than this crate.
///
/// Can be used to run `rustc_interface` queries.
/// Created by passing [`Config`] to [`run_compiler`].
pub struct Compiler {
    pub sess: Session,
    /// A reference to the current `GlobalCtxt` which we pass on to `GlobalCtxt`.
    pub(crate) current_gcx: CurrentGcx,
}

/// Converts strings provided as `--cfg [cfgspec]` into a `Cfg`.
pub(crate) fn parse_cfg(sess: &Session, cfgs: Vec<String>) -> Cfg {
    let cfg = cfgs
        .into_iter()
        .map(|s| {
            let psess = ParseSess::emitter_with_note(format!(
                "this occurred on the command line: `--cfg={s}`"
            ));
            let filename = FileName::cfg_spec_source_code(&s);

            macro_rules! error {
                ($reason: expr) => {
                    sess.dcx().fatal(format!("invalid `--cfg` argument: `{s}` ({})", $reason));
                };
            }

            match new_parser_from_source_str(&psess, filename, s.to_string(), StripTokens::Nothing)
            {
                Ok(mut parser) => {
                    parser = parser.recovery(Recovery::Forbidden);
                    match parser.parse_meta_item() {
                        Ok(meta_item)
                            if parser.token == token::Eof
                                && parser.dcx().has_errors().is_none() =>
                        {
                            if meta_item.path.segments.len() != 1 {
                                error!("argument key must be an identifier");
                            }
                            match &meta_item.kind {
                                MetaItemKind::List(..) => {}
                                MetaItemKind::NameValue(lit) if !lit.kind.is_str() => {
                                    error!("argument value must be a string");
                                }
                                MetaItemKind::NameValue(..) | MetaItemKind::Word => {
                                    let ident = meta_item.ident().expect("multi-segment cfg key");

                                    if ident.is_path_segment_keyword() {
                                        error!(
                                            "malformed `cfg` input, expected a valid identifier"
                                        );
                                    }

                                    return (ident.name, meta_item.value_str());
                                }
                            }
                        }
                        Ok(..) => {}
                        Err(err) => err.cancel(),
                    }
                }
                Err(errs) => errs.into_iter().for_each(|err| err.cancel()),
            };

            // If the user tried to use a key="value" flag, but is missing the quotes, provide
            // a hint about how to resolve this.
            if s.contains('=') && !s.contains("=\"") && !s.ends_with('"') {
                error!(concat!(
                    r#"expected `key` or `key="value"`, ensure escaping is appropriate"#,
                    r#" for your shell, try 'key="value"' or key=\"value\""#
                ));
            } else {
                error!(r#"expected `key` or `key="value"`"#);
            }
        })
        .collect::<Cfg>();

    config::build_configuration(sess, cfg)
}

/// Converts strings provided as `--check-cfg [specs]` into a `CheckCfg`.
pub(crate) fn parse_check_cfg(sess: &Session, specs: Vec<String>) -> CheckCfg {
    // If any --check-cfg is passed then exhaustive_values and exhaustive_names
    // are enabled by default.
    let exhaustive_names = !specs.is_empty();
    let exhaustive_values = !specs.is_empty();
    let mut check_cfg = CheckCfg { exhaustive_names, exhaustive_values, ..CheckCfg::default() };

    for s in specs {
        let psess = ParseSess::emitter_with_note(format!(
            "this occurred on the command line: `--check-cfg={s}`"
        ));
        let filename = FileName::cfg_spec_source_code(&s);

        const VISIT: &str =
            "visit <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more details";

        macro_rules! error {
            ($reason:expr) => {{
                let mut diag =
                    sess.dcx().struct_fatal(format!("invalid `--check-cfg` argument: `{s}`"));
                diag.note($reason);
                diag.note(VISIT);
                diag.emit()
            }};
            (in $arg:expr, $reason:expr) => {{
                let mut diag =
                    sess.dcx().struct_fatal(format!("invalid `--check-cfg` argument: `{s}`"));

                let pparg = crate::rustc_ast_pretty::pprust::meta_list_item_to_string($arg);
                if let Some(lit) = $arg.lit() {
                    let (lit_kind_article, lit_kind_descr) = {
                        let lit_kind = lit.as_token_lit().kind;
                        (lit_kind.article(), lit_kind.descr())
                    };
                    diag.note(format!("`{pparg}` is {lit_kind_article} {lit_kind_descr} literal"));
                } else {
                    diag.note(format!("`{pparg}` is invalid"));
                }

                diag.note($reason);
                diag.note(VISIT);
                diag.emit()
            }};
        }

        let expected_error = || -> ! {
            error!("expected `cfg(name, values(\"value1\", \"value2\", ... \"valueN\"))`")
        };

        let mut parser =
            match new_parser_from_source_str(&psess, filename, s.to_string(), StripTokens::Nothing)
            {
                Ok(parser) => parser.recovery(Recovery::Forbidden),
                Err(errs) => {
                    errs.into_iter().for_each(|err| err.cancel());
                    expected_error();
                }
            };

        let meta_item = match parser.parse_meta_item() {
            Ok(meta_item) if parser.token == token::Eof && parser.dcx().has_errors().is_none() => {
                meta_item
            }
            Ok(..) => expected_error(),
            Err(err) => {
                err.cancel();
                expected_error();
            }
        };

        let Some(args) = meta_item.meta_item_list() else {
            expected_error();
        };

        if !meta_item.has_name(sym::cfg) {
            expected_error();
        }

        let mut names = Vec::new();
        let mut values: FxHashSet<_> = Default::default();

        let mut any_specified = false;
        let mut values_specified = false;
        let mut values_any_specified = false;

        for arg in args {
            if arg.is_word()
                && let Some(ident) = arg.ident()
            {
                if values_specified {
                    error!("`cfg()` names cannot be after values");
                }

                if ident.is_path_segment_keyword() {
                    error!("malformed `cfg` input, expected a valid identifier");
                }

                names.push(ident);
            } else if let Some(boolean) = arg.boolean_literal() {
                if values_specified {
                    error!("`cfg()` names cannot be after values");
                }
                names.push(crate::rustc_span::Ident::new(
                    if boolean { crate::rustc_span::kw::True } else { crate::rustc_span::kw::False },
                    arg.span(),
                ));
            } else if arg.has_name(sym::any)
                && let Some(args) = arg.meta_item_list()
            {
                if any_specified {
                    error!("`any()` cannot be specified multiple times");
                }
                any_specified = true;
                if !args.is_empty() {
                    error!(in arg, "`any()` takes no argument");
                }
            } else if arg.has_name(sym::values)
                && let Some(args) = arg.meta_item_list()
            {
                if names.is_empty() {
                    error!("`values()` cannot be specified before the names");
                } else if values_specified {
                    error!("`values()` cannot be specified multiple times");
                }
                values_specified = true;

                for arg in args {
                    if let Some(LitKind::Str(s, _)) = arg.lit().map(|lit| &lit.kind) {
                        values.insert(Some(*s));
                    } else if arg.has_name(sym::any)
                        && let Some(args) = arg.meta_item_list()
                    {
                        if values_any_specified {
                            error!(in arg, "`any()` in `values()` cannot be specified multiple times");
                        }
                        values_any_specified = true;
                        if !args.is_empty() {
                            error!(in arg, "`any()` in `values()` takes no argument");
                        }
                    } else if arg.has_name(sym::none)
                        && let Some(args) = arg.meta_item_list()
                    {
                        values.insert(None);
                        if !args.is_empty() {
                            error!(in arg, "`none()` in `values()` takes no argument");
                        }
                    } else {
                        error!(in arg, "`values()` arguments must be string literals, `none()` or `any()`");
                    }
                }
            } else {
                error!(in arg, "`cfg()` arguments must be simple identifiers, `any()` or `values(...)`");
            }
        }

        if !values_specified && !any_specified {
            // `cfg(name)` is equivalent to `cfg(name, values(none()))` so add
            // an implicit `none()`
            values.insert(None);
        } else if !values.is_empty() && values_any_specified {
            error!(
                "`values()` arguments cannot specify string literals and `any()` at the same time"
            );
        }

        if any_specified {
            if names.is_empty() && values.is_empty() && !values_specified && !values_any_specified {
                check_cfg.exhaustive_names = false;
            } else {
                error!("`cfg(any())` can only be provided in isolation");
            }
        } else {
            for name in names {
                check_cfg
                    .expecteds
                    .entry(name.name)
                    .and_modify(|v| match v {
                        ExpectedValues::Some(v) if !values_any_specified =>
                        {                            v.extend(values.clone())
                        }
                        ExpectedValues::Some(_) => *v = ExpectedValues::Any,
                        ExpectedValues::Any => {}
                    })
                    .or_insert_with(|| {
                        if values_any_specified {
                            ExpectedValues::Any
                        } else {
                            ExpectedValues::Some(values.clone())
                        }
                    });
            }
        }
    }

    check_cfg.fill_well_known(&sess.target);

    check_cfg
}

/// The semantic frontend input a consumer supplies for one compilation.
///
/// Artifact paths, custom query providers, lint plugins, virtual filesystems and CLI-derived
/// output settings used to live here. This fork has no callers for them: keeping them would preserve a
/// second, hypothetical driver API beside the real one.
pub struct Config {
    /// Semantic language/session options.
    pub opts: config::Options,
    pub input: Input,
    /// Called when the parse session exists so a consumer can install its diagnostic sink.
    pub psess_created: Option<Box<dyn FnOnce(&mut ParseSess) + Send>>,
    pub using_internal_features: &'static core::sync::atomic::AtomicBool,
    /// The rustc version this session claims to be, or `None` for the one compiled in.
    ///
    /// Crate metadata carries the string of the compiler that wrote it, and loading an rlib whose
    /// string differs is E0514 "compiled by an incompatible version of rustc". The compiled-in
    /// value comes from `CFG_VERSION`, set in `.cargo/config.toml`, which means the sysroot
    /// this frontend can read is fixed when the binary is built. That is the wrong place for it:
    /// the sysroot is chosen at run time with `--sysroot`, so the string that has to match it
    /// belongs beside that choice and not inside the executable.
    pub rustc_version: Option<alloc::string::String>,
}

// JUSTIFICATION: before session exists, only config
pub fn run_compiler<R: Send>(config: Config, f: impl FnOnce(&Compiler) -> R + Send) -> R {
    trace!("run_compiler");

    // **This session's mode, first.** Everything below builds `Lock`s and `Sharded`s (the source
    // map, the interners, the `GlobalCtxt`), and each picks its synchronised or unsynchronised
    // kind from the mode when it is built, so the mode is chosen before any of them exists. It
    // is this session's own, latched on this thread until `run_compiler` returns and installed on
    // every pool thread while it runs this session's items: a serial session and a parallel one
    // can share a process and each gets the locks it runs with (see `sync::enter_session_width`).
    // It was one process-wide setting that panicked when a second session asked for the other.
    //
    // Parallel means `jobs.frontend` of two or more, with the `parallel` feature built; see
    // `util::session_width`.
    let _session_mode =
        crate::rustc_data_structures::sync::enter_session_width(util::session_width(config.opts.jobs));

    // **No jobserver.** The GNU make token protocol coordinates parallelism with an outer
    // `make`/`cargo`, and nothing in this compiler draws on it: the only consumer was
    // `jobserver::Proxy`, which gated workers in the `rustc_thread_pool` (rayon-core) pool that
    // `run_in_thread_pool_with_globals` no longer builds. What remained was a pipe this process
    // created, held one token in, and never acquired from again. Nothing here is a child of
    // that schedules its own work and is not a child of the build tool that invoked it, so an
    // inherited pool would be the wrong budget even if something did read it.
    //
    // `jobs.frontend` sizes the session's `sync::Registry`, whose slots are the session's thread
    // budget on nagoya's pool: that is the number that bounds frontend parallelism.
    let early_dcx = EarlyDiagCtxt::new(config.opts.error_format);
    let jobs = config.opts.jobs;

    let target = config::build_target_config(
        &early_dcx,
        &config.opts.target_triple,
        config.opts.sysroot.path(),
        config.opts.unstable_opts.unstable_options,
    );
    let file_loader = Box::new(RealFileLoader);
    let path_mapping = config.opts.file_path_mapping();
    // Only when asked for: the default algorithm (`src_hash_algorithm(&target)`) exists for
    // metadata, dep-info and debuginfo, none of which this crate writes.
    let hash_kind = config.opts.unstable_opts.src_hash_algorithm;
    let checksum_hash_kind = config.opts.unstable_opts.checksum_hash_algorithm();

    util::run_in_thread_pool_with_globals(
        &early_dcx,
        config.opts.edition,
        jobs,
        &[],
        SourceMapInputs { file_loader, path_mapping, hash_kind, checksum_hash_kind },
        |current_gcx| {
            let mut sess = crate::rustc_session::build_session(
                config.opts,
                CompilerIO {
                    input: config.input,
                    output_dir: None,
                    output_file: None,
                    temps_dir: None,
                },
                Default::default(),
                target,
                match config.rustc_version {
                    // Leaked because `Session::cfg_version` is `&'static str` and has to outlive
                    // the session holding it. One leak per compiler run, in a process that runs
                    // the compiler and then answers the next request with a fresh one.
                    Some(claimed) => alloc::boxed::Box::leak(claimed.into_boxed_str()),
                    None => util::rustc_version_str().unwrap_or(util::DEFAULT_CFG_VERSION),
                },
                None,
                config.using_internal_features,
            );

            // Nothing fills these, because the codegen backend that filled them upstream is not
            // part of this compiler. They are set rather than removed: upstream's monomorphization
            // collector and MIR inliner read the two intrinsic sets, and `Session::lto` reads the
            // flag, so an empty set and `false` are the answers a frontend with no backend owes
            // them. Deleting the fields would mean editing three crates to say the same thing.
            sess.replaced_intrinsics = FxHashSet::default();
            sess.fallback_intrinsics = FxHashSet::default();
            sess.thin_lto_supported = false;
            let target_config = util::frontend_target_config(&sess);

            // Store all of the target features in the session.
            // Needs to be done before `parse_cfg` because it checks this list.
            sess.internal_target_features
                .extend(target_config.internal_target_features.to_sorted_stable_ord());

            sess.config = parse_cfg(&sess, Vec::new());
            let is_nightly_build = sess.is_nightly_build();
            let is_crt_static = sess.crt_static(None);
            util::add_configuration(
                &mut sess.config,
                &target_config,
                &sess.target,
                is_nightly_build,
                is_crt_static,
            );

            sess.check_config = parse_check_cfg(&sess, Vec::new());

            if let Some(psess_created) = config.psess_created {
                psess_created(&mut sess.psess);
            }

            let lint_store = crate::rustc_lint::new_lint_store(sess.enable_internal_lints());
            sess.lint_store = Some(Arc::new(lint_store));

            util::check_abi_required_features(&sess);

            let compiler = Compiler { sess, current_gcx };

            // There are two paths out of `f`.
            // - Normal exit.
            // - Panic, e.g. triggered by `abort_if_errors` or a fatal error.
            //
            // We must run `finish_diagnostics` in both cases.
            // `panic = "abort"`: there is no unwinding, so there is nothing to catch and `f`
            // runs directly. The panic path out of `f` no longer reaches the code below - the
            // process aborts instead of running `finish_diagnostics` and `flush_delayed` on
            // the way out. That containment was dropped with `panic = "abort"`; the
            // `catch_unwind` this replaces is the thing to restore when a panic runtime
            // returns.
            let res = f(&compiler);

            compiler.sess.finish_diagnostics();

            // If error diagnostics have been emitted, we can't return an
            // error directly, because the return type of this function
            // is `R`, not `Result<R, E>`. But we need to communicate the
            // errors' existence to the caller, otherwise the caller might
            // mistakenly think that no errors occurred and return a zero
            // exit code. So we abort (panic) instead, similar to if `f`
            // had panicked.
            // `res.is_ok()` was the "`f` did not panic" test; with `panic = "abort"` reaching
            // this line already proves it, so the guard is gone.
            compiler.sess.dcx().abort_if_errors();

            // Also make sure to flush delayed bugs as if we panicked, the
            // bugs would be flushed by the Drop impl of DiagCtxt while
            // unwinding, which would result in an abort with
            // "panic in a destructor during cleanup".
            compiler.sess.dcx().flush_delayed();

            // The `Err` arm here resumed unwinding if a panic happened; with `panic = "abort"`
            // it is unreachable, so `res` is already the value.

            let prof = compiler.sess.prof.clone();
            prof.generic_activity("drop_compiler").run(move || drop(compiler));

            res
        },
    )
}

pub fn try_print_query_stack(
    dcx: DiagCtxtHandle<'_>,
    limit_frames: Option<usize>,
    file: Option<eko::file::File>,
) {
    eko::eprintln!("query stack during panic:");

    // Be careful relying on global state here: this code is called from
    // a panic hook, which means that the global `DiagCtxt` may be in a weird
    // state if it was responsible for triggering the panic.
    let all_frames = ty::tls::with_context_opt(|icx| {
        if let Some(icx) = icx {
            ty::print::with_no_queries!(print_query_stack(
                icx.tcx,
                icx.query,
                dcx,
                limit_frames,
                file,
            ))
        } else {
            0
        }
    });

    if let Some(limit_frames) = limit_frames
        && all_frames > limit_frames
    {
        eko::eprintln!(
            "... and {} other queries... use `env RUST_BACKTRACE=1` to see the full query stack",
            all_frames - limit_frames
        );
    } else {
        eko::eprintln!("end of query stack");
    }
}
