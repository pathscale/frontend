// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use core::fmt::Write as _;
use alloc::borrow::ToOwned;
use crate::rustc_data_structures::iter_ext::IterExt as _;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::any::Any;
use eko::file as io;
use eko::path::{Path, PathBuf};
use alloc::sync::Arc;
use eko::thread::{LazyLock, OnceLock};
use core::{iter};
use eko::file as fs;

use crate::rustc_ast as ast;
use crate::rustc_attr_parsing::{AttributeParser, ShouldEmit};
use crate::rustc_crate_store::Untracked;
// `FxIndexMap`, not `IndexMap`. `indexmap`'s default hasher is `RandomState`, which is std, so
// with `indexmap/std` off the hasher parameter has no default and has to be named.
use crate::rustc_data_structures::fx::FxIndexMap;
use crate::rustc_data_structures::steal::Steal;
use crate::rustc_data_structures::sync::{AppendOnlyIndexVec, FreezeLock, WorkerLocal, run_stage};
use crate::rustc_data_structures::thousands;
use crate::rustc_errors::{Diag, DiagCtxtHandle, Diagnostic, Level};
use crate::rustc_expand::base::{ExtCtxt, LintStoreExpand};
use crate::rustc_feature::Features;
use crate::rustc_fs_util::try_canonicalize;
use crate::rustc_hir::Attribute;
use crate::rustc_hir::attrs::AttributeKind;
use crate::rustc_hir::def_id::{LOCAL_CRATE, LocalModId, StableCrateId, StableCrateIdMap};
use crate::rustc_hir::definitions::Definitions;
use crate::rustc_lint::{BufferedEarlyLint, EarlyCheckNode, LintStore, unerased_lint_store};
use crate::rustc_metadata::creader::CStore;
use crate::rustc_metadata::loader::DefaultMetadataLoader;
use crate::rustc_middle::arena::Arena;
use crate::rustc_middle::ty::{self, RegisteredTools, TyCtxt};
use crate::rustc_middle::util::Providers;
use crate::rustc_parse::lexer::StripTokens;
use crate::rustc_parse::{new_parser_from_file, new_parser_from_source_str, unwrap_or_emit_fatal};
use crate::rustc_passes::{abi_test, input_stats, layout_test};
use crate::rustc_resolve::{Resolver, ResolverOutputs};
use crate::rustc_session::Session;
use crate::rustc_session::config::{Input, OutFileName, OutputFilenames, OutputType};
use crate::rustc_session::diagnostics::feature_err;
use crate::rustc_session::output::{filename_for_input, invalid_output_for_target};
use crate::rustc_session::search_paths::PathKind;
use crate::rustc_span::{
    DUMMY_SP, ErrorGuaranteed, ExpnKind, SourceFileHash, SourceFileHashAlgorithm, Span, Symbol, sym,
};
use crate::rustc_structures::{CrateType, Limit};
use crate::rustc_trait_selection::{solve, traits};
use tracing::{info, instrument};

use crate::rustc_interface::interface::Compiler;
use crate::rustc_interface::{diagnostics, limits, util};

pub fn parse<'a>(sess: &'a Session) -> ast::Crate {
    let mut krate = sess
        .time("parse_crate", || {
            let mut parser = unwrap_or_emit_fatal(match &sess.io.input {
                Input::File(file) => new_parser_from_file(
                    &sess.psess,
                    file,
                    StripTokens::ShebangAndFrontmatter,
                    None,
                ),
                Input::Str { input, name } => new_parser_from_source_str(
                    &sess.psess,
                    name.clone(),
                    input.clone(),
                    StripTokens::ShebangAndFrontmatter,
                ),
            });
            parser.parse_crate_mod()
        })
        .unwrap_or_else(|parse_error| {
            let guar: ErrorGuaranteed = parse_error.emit();
            guar.raise_fatal();
        });

    crate::rustc_builtin_macros::cmdline_attrs::inject(
        &mut krate,
        &sess.psess,
        &sess.opts.unstable_opts.crate_attr,
    );

    krate
}

fn pre_expansion_lint<'a>(
    sess: &Session,
    features: &Features,
    lint_store: &LintStore,
    registered_lint_tools: &RegisteredTools,
    check_node: EarlyCheckNode<'a>,
    node_name: Symbol,
) {
    sess.prof.generic_activity_with_arg("pre_AST_expansion_lint_checks", node_name.as_str()).run(
        || {
            crate::rustc_lint::check_ast_node(
                sess,
                features,
                true,
                lint_store,
                registered_lint_tools,
                None,
                check_node,
            );
        },
    );
}

// Cannot implement directly for `LintStore` due to trait coherence.
struct LintStoreExpandImpl<'a>(&'a LintStore);

impl LintStoreExpand for LintStoreExpandImpl<'_> {
    fn pre_expansion_lint(
        &self,
        sess: &Session,
        features: &Features,
        registered_lint_tools: &RegisteredTools,
        node_id: ast::NodeId,
        attrs: &[ast::Attribute],
        items: &[Box<ast::Item>],
        name: Symbol,
    ) {
        let check_node = EarlyCheckNode::LoadedMod(node_id, attrs, items);
        pre_expansion_lint(sess, features, self.0, registered_lint_tools, check_node, name);
    }
}

/// Runs the "early phases" of the compiler: initial `cfg` processing,
/// syntax expansion, secondary `cfg` expansion, synthesis of a test
/// harness if one is to be provided, injection of a dependency on the
/// standard library and prelude, and name resolution.
#[instrument(level = "trace", skip(krate, resolver))]
fn configure_and_expand(
    mut krate: ast::Crate,
    pre_configured_attrs: &[ast::Attribute],
    resolver: &mut Resolver<'_, '_>,
) -> ast::Crate {
    let tcx = resolver.tcx();
    let sess = tcx.sess;
    let features = tcx.features();
    let lint_store = unerased_lint_store(sess);
    let crate_name = tcx.crate_name(LOCAL_CRATE);
    pre_expansion_lint(
        sess,
        features,
        lint_store,
        tcx.registered_lint_tools(()),
        EarlyCheckNode::CrateRoot(&krate, pre_configured_attrs),
        crate_name,
    );
    crate::rustc_builtin_macros::register_builtin_macros(resolver);

    let num_standard_library_imports = sess.time("crate_injection", || {
        crate::rustc_builtin_macros::standard_library_imports::inject(
            &mut krate,
            pre_configured_attrs,
            resolver,
            sess,
            features,
        )
    });

    // Expand all macros
    krate = sess.time("macro_expand_crate", || {
        // Windows dlls do not have rpaths, so they don't know how to find their
        // dependencies. It's up to us to tell the system where to find all the
        // dependent dlls. Note that this uses cfg!(windows) as opposed to
        // targ_cfg because syntax extensions are always loaded for the host
        // compiler, not for the target.
        //
        // This is somewhat of an inherently racy operation, however, as
        // multiple threads calling this function could possibly continue
        // extending PATH far beyond what it should. To solve this for now we
        // just don't add any new elements to PATH which are already there
        // within PATH. This is basically a targeted fix at #17360 for rustdoc
        // which runs rustc in parallel but has been seen (#33844) to cause
        // problems with PATH becoming too long.
        let mut old_path = Vec::new();
        if cfg!(windows) {
            old_path = eko::env::var_os("PATH").unwrap_or(old_path);
            let mut new_path = Vec::from_iter(
                sess.host_filesearch().search_paths(PathKind::Native).map(|p| p.dir.to_path_buf()),
            );
            for path in eko::env::split_paths(&old_path) {
                let path = PathBuf::from_bytes(path.to_vec());
                if !new_path.contains(&path) {
                    new_path.push(path);
                }
            }
            // `env::join_paths` refused an entry containing the separator and this filtered
            // those out. A path with a `:` in it cannot be expressed in a `PATH` at all, so it
            // is dropped here for the same reason, without a `Result` to thread through.
            let joined: Vec<u8> = new_path
                .iter()
                .filter(|p| !p.as_bytes().contains(&b':'))
                .map(|p| p.as_bytes())
                .collect::<Vec<_>>()
                .join(&b':');
            // SAFETY: same as before - this races any other thread reading the environment, and
            // the frontend is serial here.
            unsafe {
                eko::env::set_var_bytes(b"PATH", &joined);
            }
        }

        // Create the config for macro expansion
        let recursion_limit = get_recursion_limit(pre_configured_attrs, sess);
        let cfg = crate::rustc_expand::expand::ExpansionConfig {
            crate_name,
            features,
            recursion_limit,
            trace_mac: sess.opts.unstable_opts.trace_macros,
            should_test: sess.is_test_crate(),
            span_debug: sess.opts.unstable_opts.span_debug,
            proc_macro_backtrace: sess.opts.unstable_opts.proc_macro_backtrace,
        };

        let lint_store = LintStoreExpandImpl(lint_store);
        let mut ecx = ExtCtxt::new(sess, cfg, resolver, Some(&lint_store));
        ecx.num_standard_library_imports = num_standard_library_imports;
        // Expand macros now!
        let krate = sess.time("expand_crate", || ecx.monotonic_expander().expand_crate(krate));

        if ecx.nb_macro_errors > 0 {
            sess.dcx().abort_if_errors();
        }

        // The rest is error reporting and stats

        sess.psess.buffered_lints.with_lock(|buffered_lints: &mut Vec<BufferedEarlyLint>| {
            buffered_lints.append(&mut ecx.buffered_early_lint);
        });

        sess.time("check_unused_macros", || {
            ecx.check_unused_macros();
        });

        // If we hit a recursion limit, exit early to avoid later passes getting overwhelmed
        // with a large AST
        if ecx.reduced_recursion_limit.is_some() {
            sess.dcx().abort_if_errors();
            unreachable!();
        }

        if cfg!(windows) {
            // SAFETY: races any other thread reading the environment; the frontend is serial.
            unsafe {
                eko::env::set_var_bytes(b"PATH", &old_path);
            }
        }

        if ecx.sess.opts.unstable_opts.macro_stats {
            print_macro_stats(&ecx);
        }

        krate
    });

    sess.time("maybe_building_test_harness", || {
        crate::rustc_builtin_macros::test_harness::inject(&mut krate, sess, features, resolver)
    });

    let has_proc_macro_decls = sess.time("AST_validation", || {
        crate::rustc_ast_passes::ast_validation::check_crate(
            sess,
            features,
            &krate,
            tcx.is_sdylib_interface_build(),
            resolver.lint_buffer(),
        )
    });

    let crate_types = tcx.crate_types();
    let is_executable_crate = crate_types.contains(&CrateType::Executable);
    let is_proc_macro_crate = crate_types.contains(&CrateType::ProcMacro);

    if crate_types.len() > 1 {
        if is_executable_crate {
            sess.dcx().emit_err(diagnostics::MixedBinCrate);
        }
        if is_proc_macro_crate {
            sess.dcx().emit_err(diagnostics::MixedProcMacroCrate);
        }
    }

    if is_proc_macro_crate && sess.target.is_like_wasm && !sess.opts.unstable_opts.wasm_proc_macros
    {
        sess.dcx().emit_err(diagnostics::UnstableWasmProcMacro);
    }

    if crate_types.contains(&CrateType::Sdylib) && !tcx.features().export_stable() {
        feature_err(sess, sym::export_stable, DUMMY_SP, "`sdylib` crate type is unstable").emit();
    }

    if is_proc_macro_crate && !sess.panic_strategy().unwinds() {
        sess.dcx().emit_warn(diagnostics::ProcMacroCratePanicAbort);
    }

    sess.time("maybe_create_a_macro_crate", || {
        let is_test_crate = sess.is_test_crate();
        crate::rustc_builtin_macros::proc_macro_harness::inject(
            &mut krate,
            sess,
            features,
            resolver,
            is_proc_macro_crate,
            has_proc_macro_decls,
            is_test_crate,
            sess.dcx(),
        )
    });

    // Done with macro expansion!

    resolver.resolve_crate(&krate);

    CStore::from_tcx(tcx).report_session_incompatibilities(tcx, &krate);
    krate
}

fn print_macro_stats(ecx: &ExtCtxt<'_>) {
    use core::fmt::Write;

    let crate_name = ecx.ecfg.crate_name.as_str();
    let crate_name = if crate_name == "build_script_build" {
        // This is a build script. Get the package name from the environment.
        let pkg_name =
            eko::env::var("CARGO_PKG_NAME").unwrap_or_else(|| "<unknown crate>".to_string());
        format!("{pkg_name} build script")
    } else {
        crate_name.to_string()
    };

    // No instability because we immediately sort the produced vector.
    let mut macro_stats: Vec<_> = ecx
        .macro_stats
        .iter()
        .map(|((name, kind), stat)| {
            // This gives the desired sort order: sort by bytes, then lines, etc.
            (stat.bytes, stat.lines, stat.uses, name, *kind)
        })
        .collect();
    macro_stats.sort_unstable();
    macro_stats.reverse(); // bigger items first

    let prefix = "macro-stats";
    let name_w = 32;
    let uses_w = 7;
    let lines_w = 11;
    let avg_lines_w = 11;
    let bytes_w = 11;
    let avg_bytes_w = 11;
    let banner_w = name_w + uses_w + lines_w + avg_lines_w + bytes_w + avg_bytes_w;

    // We write all the text into a string and print it with a single
    // `eprint!`. This is an attempt to minimize interleaved text if multiple
    // rustc processes are printing macro-stats at the same time (e.g. with
    // `RUSTFLAGS='-Zmacro-stats' cargo build`). It still doesn't guarantee
    // non-interleaving, though.
    let mut s = String::new();
    _ = writeln!(s, "{prefix} {}", "=".repeat(banner_w));
    _ = writeln!(s, "{prefix} MACRO EXPANSION STATS: {}", crate_name);
    _ = writeln!(
        s,
        "{prefix} {:<name_w$}{:>uses_w$}{:>lines_w$}{:>avg_lines_w$}{:>bytes_w$}{:>avg_bytes_w$}",
        "Macro Name", "Uses", "Lines", "Avg Lines", "Bytes", "Avg Bytes",
    );
    _ = writeln!(s, "{prefix} {}", "-".repeat(banner_w));
    // It's helpful to print something when there are no entries, otherwise it
    // might look like something went wrong.
    if macro_stats.is_empty() {
        _ = writeln!(s, "{prefix} (none)");
    }
    for (bytes, lines, uses, name, kind) in macro_stats {
        let mut name = ExpnKind::Macro(kind, *name).descr();
        let uses_with_underscores = thousands::usize_with_underscores(uses);
        let avg_lines = lines as f64 / uses as f64;
        let avg_bytes = bytes as f64 / uses as f64;

        // Ensure the "Macro Name" and "Uses" columns are as compact as possible.
        let mut uses_w = uses_w;
        if name.len() + uses_with_underscores.len() >= name_w + uses_w {
            // The name would abut or overlap the uses value. Print the name
            // on a line by itself, then set the name to empty and print things
            // normally, to show the stats on the next line.
            _ = writeln!(s, "{prefix} {:<name_w$}", name);
            name = String::new();
        } else if name.len() >= name_w {
            // The name won't abut or overlap with the uses value, but it does
            // overlap with the empty part of the uses column. Shrink the width
            // of the uses column to account for the excess name length.
            uses_w -= name.len() - name_w;
        };

        _ = writeln!(
            s,
            "{prefix} {:<name_w$}{:>uses_w$}{:>lines_w$}{:>avg_lines_w$}{:>bytes_w$}{:>avg_bytes_w$}",
            name,
            uses_with_underscores,
            thousands::usize_with_underscores(lines),
            thousands::f64p1_with_underscores(avg_lines),
            thousands::usize_with_underscores(bytes),
            thousands::f64p1_with_underscores(avg_bytes),
        );
    }
    _ = writeln!(s, "{prefix} {}", "=".repeat(banner_w));
    eko::eprint!("{s}");
}

fn early_lint_checks(tcx: TyCtxt<'_>, (): ()) {
    let sess = tcx.sess;
    let (resolver, krate) = tcx.resolver_for_lowering();
    let resolver = &*resolver.borrow();
    let krate = &*krate.borrow();
    let mut lint_buffer = resolver.lint_buffer.steal();

    if sess.opts.unstable_opts.input_stats {
        input_stats::print_ast_stats(tcx, krate);
    }

    // Needs to go *after* expansion to be able to check the results of macro expansion.
    sess.time("complete_gated_feature_checking", || {
        crate::rustc_ast_passes::feature_gate::check_crate(krate, sess, tcx.features());
    });

    // Add all buffered lints from the `ParseSess` to the `Session`.
    sess.psess.buffered_lints.with_lock(|buffered_lints| {
        info!("{} parse sess buffered_lints", buffered_lints.len());
        for early_lint in buffered_lints.drain(..) {
            lint_buffer.add_early_lint(early_lint);
        }
    });

    // Gate identifiers containing invalid Unicode codepoints that were recovered during lexing.
    sess.psess.bad_unicode_identifiers.with_lock(|identifiers| {
        for (ident, mut spans) in identifiers.drain(..) {
            spans.sort();
            if ident == sym::ferris {
                enum FerrisFix {
                    SnakeCase,
                    ScreamingSnakeCase,
                    PascalCase,
                }

                impl FerrisFix {
                    const fn as_str(self) -> &'static str {
                        match self {
                            FerrisFix::SnakeCase => "ferris",
                            FerrisFix::ScreamingSnakeCase => "FERRIS",
                            FerrisFix::PascalCase => "Ferris",
                        }
                    }
                }

                let first_span = spans[0];
                let prev_source = sess.psess.source_map().span_to_prev_source(first_span);
                let ferris_fix = prev_source
                    .map_or(FerrisFix::SnakeCase, |source| {
                        let mut source_before_ferris = source.split_whitespace().rev();
                        match source_before_ferris.next() {
                            Some("struct" | "trait" | "mod" | "union" | "type" | "enum") => {
                                FerrisFix::PascalCase
                            }
                            Some("const" | "static") => FerrisFix::ScreamingSnakeCase,
                            Some("mut") if source_before_ferris.next() == Some("static") => {
                                FerrisFix::ScreamingSnakeCase
                            }
                            _ => FerrisFix::SnakeCase,
                        }
                    })
                    .as_str();

                sess.dcx().emit_err(diagnostics::FerrisIdentifier {
                    spans,
                    first_span,
                    ferris_fix,
                });
            } else {
                sess.dcx().emit_err(diagnostics::EmojiIdentifier { spans, ident });
            }
        }
    });

    let lint_store = unerased_lint_store(tcx.sess);
    crate::rustc_lint::check_ast_node(
        sess,
        tcx.features(),
        false,
        lint_store,
        tcx.registered_lint_tools(()),
        Some(lint_buffer),
        EarlyCheckNode::CrateRoot(&*krate, &*krate.attrs),
    )
}

// Behaviour change: `OsStr` is a `std` type, so the key and the value are now plain byte
// slices. The query declaration in `rustc_middle/src/queries.rs` has to agree.
fn env_var_os<'tcx>(tcx: TyCtxt<'tcx>, key: &'tcx [u8]) -> Option<&'tcx [u8]> {
    let value = core::str::from_utf8(key).ok().and_then(eko::env::var_os);

    let value_tcx = value.as_ref().map(|value| {
        // The value is already bytes, so it goes into the arena as it stands. This used to go
        // out through `OsStr::as_encoded_bytes` and back through
        // `from_encoded_bytes_unchecked`, with a `debug_assert` that the round trip was
        // faithful - all of which existed to get an `OsStr` back out of a byte slice.
        let encoded_bytes: &'tcx [u8] = tcx.arena.alloc_slice(value);
        encoded_bytes
    });

    // Also add the variable to Cargo's dependency tracking
    //
    // NOTE: This only works for passes run before `write_dep_info`. See that
    // for extension points for configuring environment variables to be
    // properly change-tracked.
    tcx.sess.env_depinfo.borrow_mut().insert((
        Symbol::intern(&String::from_utf8_lossy(key)),
        value.as_ref().and_then(|v| core::str::from_utf8(v).ok()).map(Symbol::intern),
    ));

    value_tcx
}

// Returns all the paths that correspond to generated files.
fn generated_output_paths(
    tcx: TyCtxt<'_>,
    outputs: &OutputFilenames,
    exact_name: bool,
    crate_name: Symbol,
) -> Vec<PathBuf> {
    let sess = tcx.sess;
    let mut out_filenames = Vec::new();
    for output_type in sess.opts.output_types.keys() {
        let out_filename = outputs.path(*output_type);
        let file = out_filename.as_path().to_path_buf();
        match *output_type {
            // If the filename has been overridden using `-o`, it will not be modified
            // by appending `.rlib`, `.exe`, etc., so we can skip this transformation.
            OutputType::Exe if !exact_name => {
                for crate_type in tcx.crate_types().iter() {
                    let p = filename_for_input(sess, *crate_type, crate_name, outputs);
                    out_filenames.push(p.as_path().to_path_buf());
                }
            }
            OutputType::DepInfo if sess.opts.unstable_opts.dep_info_omit_d_target => {
                // Don't add the dep-info output when omitting it from dep-info targets
            }
            OutputType::DepInfo if out_filename.is_stdout() => {
                // Don't add the dep-info output when it goes to stdout
            }
            _ => {
                out_filenames.push(file);
            }
        }
    }
    out_filenames
}

fn output_contains_path(output_paths: &[PathBuf], input_path: &Path) -> bool {
    let input_path = try_canonicalize(input_path).ok();
    if input_path.is_none() {
        return false;
    }
    output_paths.iter().any(|output_path| try_canonicalize(output_path).ok() == input_path)
}

fn output_conflicts_with_dir(output_paths: &[PathBuf]) -> Option<&PathBuf> {
    output_paths.iter().find(|output_path| output_path.is_dir())
}

fn escape_dep_filename(filename: &str) -> String {
    // Apparently clang and gcc *only* escape spaces:
    // https://llvm.org/klaus/clang/commit/9d50634cfc268ecc9a7250226dd5ca0e945240d4
    filename.replace(' ', "\\ ")
}

// Makefile comments only need escaping newlines and `\`.
// The result can be unescaped by anything that can unescape `escape_default` and friends.
fn escape_dep_env(symbol: Symbol) -> String {
    let s = symbol.as_str();
    let mut escaped = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => escaped.push_str(r"\n"),
            '\r' => escaped.push_str(r"\r"),
            '\\' => escaped.push_str(r"\\"),
            _ => escaped.push(c),
        }
    }
    escaped
}

fn write_out_deps(tcx: TyCtxt<'_>, outputs: &OutputFilenames, out_filenames: &[PathBuf]) {
    // Write out dependency rules to the dep-info file if requested
    let sess = tcx.sess;
    if !sess.opts.output_types.contains_key(&OutputType::DepInfo) {
        return;
    }
    let deps_output = outputs.path(OutputType::DepInfo);
    let deps_filename = deps_output.as_path();

    // Immediately called closure in place of an unstable `try {}` block.
    let result = (|| -> Result<(), core::fmt::Error> {
        // Build a list of files used to compile the output and
        // write Makefile-compatible dependency rules
        let mut files: FxIndexMap<String, (u64, Option<SourceFileHash>)> = sess
            .source_map()
            .files()
            .iter()
            .filter(|fmap| fmap.is_real_file())
            .filter(|fmap| !fmap.is_imported())
            .map(|fmap| {
                (
                    escape_dep_filename(&fmap.name.prefer_local_unconditionally().to_string()),
                    (
                        // This needs to be unnormalized,
                        // as external tools wouldn't know how rustc normalizes them
                        fmap.unnormalized_source_len as u64,
                        fmap.checksum_hash,
                    ),
                )
            })
            .collect();

        let checksum_hash_algo = sess.opts.unstable_opts.checksum_hash_algorithm;

        // Account for explicitly marked-to-track files
        // (e.g. accessed in proc macros).
        let file_depinfo = sess.file_depinfo.borrow();

        let normalize_path = |path: PathBuf| escape_dep_filename(&path.to_string_lossy());

        // The entries will be used to declare dependencies between files in a
        // Makefile-like output, so the iteration order does not matter.
        fn hash_iter_files<P: AsRef<Path>>(
            it: impl Iterator<Item = P>,
            checksum_hash_algo: Option<SourceFileHashAlgorithm>,
        ) -> impl Iterator<Item = (P, (u64, Option<SourceFileHash>))> {
            it.map(move |path| {
                match checksum_hash_algo.and_then(|algo| {
                    // Read once and hash the bytes, rather than streaming the file and then
                    // asking the filesystem how long it was - two answers that could disagree
                    // if the file changed between them.
                    fs::read(path.as_ref())
                        .map(|bytes| {
                            let h = SourceFileHash::new_in_memory(algo, &bytes);
                            (bytes.len() as u64, h)
                        })
                        .map_err(|e| {
                            tracing::error!(
                                "failed to compute checksum, omitting it from dep-info {} {e}",
                                path.as_ref().display()
                            )
                        })
                        .ok()
                }) {
                    Some((file_len, checksum)) => (path, (file_len, Some(checksum))),
                    None => (path, (0, None)),
                }
            })
        }

        let extra_tracked_files = hash_iter_files(
            file_depinfo.iter().map(|path_sym| normalize_path(PathBuf::from(path_sym.as_str()))),
            checksum_hash_algo,
        );
        files.extend(extra_tracked_files);

        // We also need to track used PGO profile files
        if let Some(ref profile_instr) = sess.opts.cg.profile_use {
            files.extend(hash_iter_files(
                iter::once(normalize_path(profile_instr.as_path().to_path_buf())),
                checksum_hash_algo,
            ));
        }
        if let Some(ref profile_sample) = sess.opts.cg.profile_sample_use {
            files.extend(hash_iter_files(
                iter::once(normalize_path(profile_sample.as_path().to_path_buf())),
                checksum_hash_algo,
            ));
        }

        // Debugger visualizer files
        for debugger_visualizer in tcx.debugger_visualizers(LOCAL_CRATE) {
            files.extend(hash_iter_files(
                iter::once(normalize_path(debugger_visualizer.path.clone().unwrap())),
                checksum_hash_algo,
            ));
        }

        if sess.binary_dep_depinfo() {
            if let Some(ref backend) = sess.opts.unstable_opts.codegen_backend {
                if backend.contains('.') {
                    // If the backend name contain a `.`, it is the path to an external dynamic
                    // library. If not, it is not a path.
                    files.extend(hash_iter_files(
                        iter::once(backend.to_string()),
                        checksum_hash_algo,
                    ));
                }
            }

            for &cnum in tcx.crates(()) {
                let source = tcx.used_crate_source(cnum);
                if let Some(path) = &source.dylib {
                    files.extend(hash_iter_files(
                        iter::once(escape_dep_filename(&path.display().to_string())),
                        checksum_hash_algo,
                    ));
                }
                if let Some(path) = &source.rlib {
                    files.extend(hash_iter_files(
                        iter::once(escape_dep_filename(&path.display().to_string())),
                        checksum_hash_algo,
                    ));
                }
                if let Some(path) = &source.rmeta {
                    files.extend(hash_iter_files(
                        iter::once(escape_dep_filename(&path.display().to_string())),
                        checksum_hash_algo,
                    ));
                }
            }
        }

        let write_deps_to_file = |file: &mut dyn core::fmt::Write| -> core::fmt::Result {
            for path in out_filenames {
                writeln!(
                    file,
                    "{}: {}\n",
                    path.display(),
                    files.keys().map(String::as_str).separated_by(" ").collect::<String>()
                )?;
            }

            // Emit a fake target for each input file to the compilation. This
            // prevents `make` from spitting out an error if a file is later
            // deleted. For more info see #28735
            for path in files.keys() {
                writeln!(file, "{path}:")?;
            }

            // Emit special comments with information about accessed environment variables.
            let env_depinfo = sess.env_depinfo.borrow();
            if !env_depinfo.is_empty() {
                // We will soon sort, so the initial order does not matter.
                let mut envs: Vec<_> = env_depinfo
                    .iter()
                    .map(|(k, v)| (escape_dep_env(*k), v.map(escape_dep_env)))
                    .collect();
                envs.sort_unstable();
                writeln!(file)?;
                for (k, v) in envs {
                    write!(file, "# env-dep:{k}")?;
                    if let Some(v) = v {
                        write!(file, "={v}")?;
                    }
                    writeln!(file)?;
                }
            }

            // If caller requested this information, add special comments about source file checksums.
            // These are not necessarily the same checksums as was used in the debug files.
            if sess.opts.unstable_opts.checksum_hash_algorithm().is_some() {
                files
                    .iter()
                    .filter_map(|(path, (file_len, hash_algo))| {
                        hash_algo.map(|hash_algo| (path, file_len, hash_algo))
                    })
                    .try_for_each(|(path, file_len, checksum_hash)| {
                        writeln!(file, "# checksum:{checksum_hash} file_len:{file_len} {path}")
                    })?;
            }

            Ok(())
        };

        match deps_output {
            OutFileName::Stdout => {
                // No `BufWriter`: `Stream` writes each formatted chunk straight out, and a
                // dep-info file is small enough that the syscalls do not matter.
                let mut file = eko::file::stdout();
                write_deps_to_file(&mut file)?;
            }
            OutFileName::Real(ref path) => {
                let mut file = fs::File::create(path).map_err(|_| core::fmt::Error)?;
                write_deps_to_file(&mut file)?;
            }
        }
        Ok(())
    })();

    match result {
        Ok(_) => {
            if sess.opts.json_artifact_notifications {
                sess.dcx().emit_artifact_notification(deps_filename, "dep-info");
            }
        }
        Err(error) => {
            sess.dcx()
                .emit_fatal(diagnostics::ErrorWritingDependencies { path: deps_filename, error });
        }
    }
}

fn resolver_for_lowering_raw<'tcx>(
    tcx: TyCtxt<'tcx>,
    (): (),
) -> (
    &'tcx Steal<ty::ResolverAstLowering<'tcx>>,
    &'tcx Steal<ast::Crate>,
    &'tcx ty::ResolverGlobalCtxt,
) {
    let arenas = WorkerLocal::new(|_| Resolver::arenas());
    let _ = tcx.registered_attr_tools(()); // Uses `crate_for_resolver`.
    let _ = tcx.registered_lint_tools(()); // Uses `crate_for_resolver`.
    let (krate, pre_configured_attrs) = tcx.crate_for_resolver(()).steal();
    let mut resolver = Resolver::new(
        tcx,
        &pre_configured_attrs,
        krate.spans.inner_span,
        krate.spans.inject_use_span,
        &arenas,
    );
    let krate = configure_and_expand(krate, &pre_configured_attrs, &mut resolver);

    // Don't mutate the cstore or stable crate id map from here on.
    tcx.untracked().freeze_cstore();

    let ResolverOutputs {
        global_ctxt: untracked_resolutions,
        ast_lowering: untracked_resolver_for_lowering,
    } = resolver.into_outputs();

    (
        tcx.arena.alloc(Steal::new(untracked_resolver_for_lowering)),
        tcx.arena.alloc(Steal::new(krate)),
        tcx.arena.alloc(untracked_resolutions),
    )
}

pub fn write_dep_info(tcx: TyCtxt<'_>) {
    // Make sure name resolution and macro expansion is run for
    // the side-effect of providing a complete set of all
    // accessed files and env vars.
    let _ = tcx.resolver_for_lowering();

    let sess = tcx.sess;
    let _timer = sess.timer("write_dep_info");
    let crate_name = tcx.crate_name(LOCAL_CRATE);

    let outputs = tcx.output_filenames(());
    let output_paths =
        generated_output_paths(tcx, outputs, sess.io.output_file.is_some(), crate_name);

    // Ensure the source file isn't accidentally overwritten during compilation.
    if let Some(input_path) = sess.io.input.opt_path() {
        if sess.opts.will_create_output_file() {
            if output_contains_path(&output_paths, input_path) {
                sess.dcx()
                    .emit_fatal(diagnostics::InputFileWouldBeOverWritten { path: input_path });
            }
            if let Some(dir_path) = output_conflicts_with_dir(&output_paths) {
                sess.dcx().emit_fatal(diagnostics::GeneratedFileConflictsWithDirectory {
                    input_path,
                    dir_path,
                });
            }
        }
    }

    if let Some(ref dir) = sess.io.temps_dir {
        if fs::create_dir_all(dir).is_err() {
            sess.dcx().emit_fatal(diagnostics::TempsDirError);
        }
    }

    write_out_deps(tcx, outputs, &output_paths);

    let only_dep_info = sess.opts.output_types.contains_key(&OutputType::DepInfo)
        && sess.opts.output_types.len() == 1;

    if !only_dep_info {
        if let Some(ref dir) = sess.io.output_dir {
            if fs::create_dir_all(dir).is_err() {
                sess.dcx().emit_fatal(diagnostics::OutDirError);
            }
        }
    }
}

pub fn write_interface<'tcx>(tcx: TyCtxt<'tcx>) {
    if !tcx.crate_types().contains(&crate::rustc_structures::CrateType::Sdylib) {
        return;
    }
    let _timer = tcx.sess.timer("write_interface");
    let (_, krate) = tcx.resolver_for_lowering();

    let krate = crate::rustc_ast_pretty::pprust::print_crate_as_interface(
        &*krate.borrow(),
        tcx.sess.psess.edition,
        &tcx.sess.psess.attr_id_generator,
    );
    let export_output = tcx.output_filenames(()).interface_path();
    let mut file = fs::File::create(&export_output).unwrap_or_else(|error| {
        tcx.dcx().emit_fatal(diagnostics::FailedWritingFile { path: &export_output, error })
    });
    // `write!` on a `File` goes through `core::fmt::Write`, whose error carries nothing, while
    // the diagnostic wants the file error. The write is turned back into one: a failure here is
    // always the underlying `write(2)`, since formatting itself cannot fail.
    if write!(file, "{}", krate).is_err() {
        let error = eko::file::Error::other("could not write the interface file");
        tcx.dcx().emit_fatal(diagnostics::FailedWritingFile { path: &export_output, error });
    }
}

/// Every item in the crate, with every HIR owner lowered first, as one stage.
///
/// `hir_crate_items` is the query every HIR consumer passes through before it reads an owner:
/// `analysis` forces it before its first stage, and the facts extractor forces it first thing.
/// Its walk (`rustc_middle::hir::map::hir_crate_items`) visits every owner from the crate root
/// and used to lower each one, serially, the first time it touched it. Lowering is per owner
/// (`lower_to_hir` is a query per `LocalDefId`) and one owner's lowering writes nothing another
/// reads except through queries, so it is done up front instead, as a stage over the AST index
/// (`rustc_ast_lowering::lower_every_owner`, whose header has the checks), and the walk then
/// reads finished results. In a serial session the stage is a loop in index order on this
/// thread, which is the same lowering in a slightly different order: index order is the def
/// collector's pre-order walk, the old order was the HIR walk's.
fn hir_crate_items(tcx: TyCtxt<'_>, (): ()) -> crate::rustc_middle::hir::ModuleItems {
    crate::rustc_ast_lowering::lower_every_owner(tcx);
    crate::rustc_middle::hir::map::hir_crate_items(tcx, ())
}

pub static DEFAULT_QUERY_PROVIDERS: LazyLock<Providers> = LazyLock::new(|| {
    let providers = &mut Providers::default();
    providers.queries.analysis = analysis;
    providers.queries.resolver_for_lowering_raw = resolver_for_lowering_raw;
    providers.queries.stripped_cfg_items = |tcx, _| &tcx.resolutions(()).stripped_cfg_items[..];
    providers.queries.resolutions = |tcx, ()| tcx.resolver_for_lowering_raw(()).2;
    providers.queries.early_lint_checks = early_lint_checks;
    providers.queries.env_var_os = env_var_os;
    providers.queries.proc_macro_decls_static = |tcx, _| tcx.hir_crate_items(()).proc_macro_decls();
    crate::rustc_ast_lowering::provide(&mut providers.queries);
    limits::provide(&mut providers.queries);
    crate::rustc_expand::provide(&mut providers.queries);
    crate::rustc_const_eval::provide(providers);
    crate::rustc_middle::hir::provide(&mut providers.queries);
    // After `rustc_middle::hir::provide`, which sets the plain one. See `hir_crate_items` below.
    providers.queries.hir_crate_items = hir_crate_items;
    crate::rustc_borrowck::provide(&mut providers.queries);
    // The dependency graph is disabled, so there is nothing to serialise at session teardown.
    // Saving it would write an empty graph and then read it back as a cache miss on every query.
    providers.hooks.save_dep_graph = |_tcx| {};
    crate::rustc_mir_build::provide(providers);
    crate::rustc_mir_transform::provide(providers);
    crate::rustc_monomorphize::provide(providers);
    crate::rustc_privacy::provide(&mut providers.queries);
    crate::rustc_query_impl::provide(providers);
    crate::rustc_resolve::provide(&mut providers.queries);
    crate::rustc_hir_analysis::provide(&mut providers.queries);
    crate::rustc_hir_typeck::provide(&mut providers.queries);
    ty::provide(&mut providers.queries);
    traits::provide(&mut providers.queries);
    solve::provide(&mut providers.queries);
    crate::rustc_passes::provide(&mut providers.queries);
    crate::rustc_traits::provide(&mut providers.queries);
    crate::rustc_ty_utils::provide(&mut providers.queries);
    crate::rustc_metadata::provide(providers);
    crate::rustc_lint::provide(&mut providers.queries);
    crate::rustc_symbol_mangling::provide(&mut providers.queries);
    crate::frontend_semantics::provide(providers);
    *providers
});

pub fn create_and_enter_global_ctxt<T, F: for<'tcx> FnOnce(TyCtxt<'tcx>) -> T>(
    compiler: &Compiler,
    krate: crate::rustc_ast::Crate,
    f: F,
) -> T {
    let sess = &compiler.sess;

    let pre_configured_attrs = crate::rustc_expand::config::pre_configure_attrs(sess, &krate.attrs);

    let crate_name = get_crate_name(sess, &pre_configured_attrs);
    // The two crate shapes this frontend accepts: a library to analyse, and a program to analyse.
    // Nothing here writes an object or runs a linker, so `cdylib`, `staticlib`, `dylib` and
    // `proc-macro` have no meaning to give them and are not offered.
    let crate_types = collect_crate_types(
        sess,
        &[CrateType::Rlib, CrateType::Executable],
        // The `supported_by` label, which appears verbatim in "dropping unsupported crate type"
        // diagnostics. It names whatever is driving this frontend, and the frontend itself is the
        // honest answer here.
        "this frontend",
        &pre_configured_attrs,
        krate.spans.inner_span,
    );
    let stable_crate_id = StableCrateId::new(
        crate_name,
        crate_types.contains(&CrateType::Executable),
        sess.opts.cg.metadata.clone(),
        sess.cfg_version,
    );

    let outputs = util::build_output_filenames(&pre_configured_attrs, sess);

    // Each request gets its own arena-bound query context, and the dependency graph is disabled.
    //
    // rustc's incremental machinery exists to coordinate *separate processes* through a cache on
    // disk: the graph records what a previous run computed so the next run can skip it. A caller
    // that keeps this compiler alive and hands it one request after another has that continuity
    // already, in memory, and paying for the on-disk graph as well would be paying twice for it.
    let dep_graph = crate::rustc_middle::dep_graph::DepGraph::new_disabled();
    let cstore = FreezeLock::new(Box::new(CStore::new(Box::new(DefaultMetadataLoader))) as _);
    let definitions = FreezeLock::new(Definitions::new(stable_crate_id));

    let stable_crate_ids = FreezeLock::new(StableCrateIdMap::default());
    let untracked =
        Untracked { cstore, source_span: AppendOnlyIndexVec::new(), definitions, stable_crate_ids };

    // We're constructing the HIR here; we don't care what we will
    // read, since we haven't even constructed the *input* to
    // incr. comp. yet.
    dep_graph.assert_ignored();

    let query_result_on_disk_cache = None;

    let providers = *DEFAULT_QUERY_PROVIDERS;

    let incremental = dep_graph.is_fully_enabled();

    // Note: this function body is the origin point of the widely-used 'tcx lifetime.
    //
    // `gcx_cell` is defined here and `&gcx_cell` is passed to `create_global_ctxt`, which then
    // actually creates the `GlobalCtxt` with a `gcx_cell.get_or_init(...)` call. This is done so
    // that the resulting reference has the type `&'tcx GlobalCtxt<'tcx>`, which is what `TyCtxt`
    // needs. If we defined and created the `GlobalCtxt` within `create_global_ctxt` then its type
    // would be `&'a GlobalCtxt<'tcx>`, with two lifetimes.
    //
    // Similarly, by creating `arena` here and passing in `&arena`, that reference has the type
    // `&'tcx WorkerLocal<Arena<'tcx>>`, also with one lifetime. And likewise for `hir_arena`.

    // Each of these three is borrowed for `'tcx`, and `'tcx` also appears in its own type, so it
    // is self-referential. Owned as ordinary locals, dropping one runs a destructor that dropck
    // assumes may touch `'tcx` data, and it rejects that (E0597) unless the destructor is
    // `#[may_dangle]`, which is `dropck_eyepatch`, unstable. Upstream's `Anchor` made exactly
    // that promise with `unsafe impl<#[may_dangle] T> Drop`.
    //
    // This makes the same promise on stable, as narrowly: each owner lives on the heap behind a
    // `FreeOnDrop`, which holds a raw pointer and so carries no lifetime for dropck to check.
    // The guards are declared arena, hir arena, global context, so they free in the reverse
    // order: the global context first, while both arenas it points into are still alive, then
    // the arenas. Nothing is leaked. It is sound because nothing borrowing `'tcx` outlives this
    // function: `f` returns a `T` chosen for every `'tcx`, so it cannot hold one.
    struct FreeOnDrop<T>(*mut T);
    impl<T> Drop for FreeOnDrop<T> {
        fn drop(&mut self) {
            // SAFETY: the pointer came from `Box::into_raw` below and is freed only here, once,
            // after every `'tcx` borrow of it has ended (see above).
            drop(unsafe { Box::from_raw(self.0) });
        }
    }
    let arena_owner = FreeOnDrop(Box::into_raw(Box::new(WorkerLocal::new(|_| Arena::default()))));
    let hir_arena_owner = FreeOnDrop(Box::into_raw(Box::new(WorkerLocal::new(|_| {
        crate::rustc_hir::Arena::default()
    }))));
    let gcx_owner = FreeOnDrop(Box::into_raw(Box::new(OnceLock::new())));
    // SAFETY: each pointer is live until its guard drops at the end of this function.
    let arena = unsafe { &*arena_owner.0 };
    let hir_arena = unsafe { &*hir_arena_owner.0 };
    let gcx_cell = unsafe { &*gcx_owner.0 };

    let res = TyCtxt::create_global_ctxt(
        gcx_cell,
        &compiler.sess,
        crate_types,
        stable_crate_id,
        arena,
        hir_arena,
        untracked,
        None,
        dep_graph,
        crate::rustc_query_impl::query_system(
            arena,
            providers.queries,
            providers.extern_queries,
            query_result_on_disk_cache,
            incremental,
        ),
        providers.hooks,
        compiler.current_gcx.clone(),
        |tcx| {
            let feed = tcx.create_crate_num(stable_crate_id).unwrap();
            assert_eq!(feed.key(), LOCAL_CRATE);
            feed.crate_name(crate_name);

            let feed = tcx.feed_unit_query();
            feed.features_query(tcx.arena.alloc(crate::rustc_expand::config::features(
                tcx.sess,
                &pre_configured_attrs,
                crate_name,
            )));
            feed.crate_for_resolver(tcx.arena.alloc(Steal::new((krate, pre_configured_attrs))));
            feed.output_filenames(Arc::new(outputs));

            // There is now one path out of `f`: normal exit. A panic (e.g. triggered by
            // `abort_if_errors` or a fatal error) used to be caught here so the self-profiler
            // could be wound down - otherwise the queries still in flight are recorded as
            // "<unknown>" in the profiling data - and then re-raised with `resume_unwind`.
            //
            // Panic containment was dropped with `panic = "abort"`: a panic aborts the process,
            // so there is nothing to catch and nothing to resume, and the
            // `tcx.alloc_self_profile_query_strings()` wind-down on the panic path is now
            // unreachable. Restore both when a panic runtime exists again. (That wind-down has
            // since been deleted outright along with `measureme`, so only the catch is left to
            // restore.)
            let res = f(tcx);

            tcx.finish();
            res
        },
    );

    res
}

struct DiagCallback<'tcx> {
    callback: Box<
        // `+ DynSend + DynSync` dropped: no longer auto traits (see
        // `rustc_data_structures/marker.rs`).
        dyn for<'b> FnOnce(DiagCtxtHandle<'b>, Level, &dyn Any) -> Diag<'b, ()>,
    >,
    tcx: TyCtxt<'tcx>,
}

impl<'a, 'tcx> Diagnostic<'a, ()> for DiagCallback<'tcx> {
    fn into_diag(self, dcx: DiagCtxtHandle<'a>, level: Level) -> Diag<'a, ()> {
        (self.callback)(dcx, level, self.tcx.sess)
    }
}

pub fn emit_delayed_lints(tcx: TyCtxt<'_>) {
    for owner_id in tcx.hir_crate_items(()).owners() {
        if let Some(delayed_lints) = tcx.opt_ast_lowering_delayed_lints(owner_id) {
            for lint in delayed_lints.steal() {
                tcx.emit_node_span_lint(
                    lint.lint_id.lint,
                    lint.id,
                    lint.span.clone(),
                    DiagCallback { callback: lint.callback, tcx },
                );
            }
        }
    }
}

/// Runs all analyses that we guarantee to run, even if errors were reported in earlier analyses.
/// This function never fails.
fn run_required_analyses(tcx: TyCtxt<'_>) {
    if tcx.sess.opts.unstable_opts.input_stats {
        crate::rustc_passes::input_stats::print_hir_stats(tcx);
    }
    // When using rustdoc's "jump to def" feature, it enters this code and `check_crate`
    // is not defined. So we need to cfg it out.
    #[cfg(all(not(doc), debug_assertions))]
    crate::rustc_passes::hir_id_validator::check_crate(tcx);

    // Prefetch this to prevent multiple threads from blocking on it later.
    // This is needed since the `hir_id_validator::check_crate` call above is not guaranteed
    // to use `hir_crate_items`.
    tcx.ensure_done().hir_crate_items(());

    crate::rustc_passes::delegation::check_glob_and_list_delegations_target_expr(tcx);

    let sess = tcx.sess;
    sess.time("misc_checking_1", || {
        // Three independent checks, as one stage whose input is the checks themselves: item `i`
        // runs `checks[i]`. Serially they run in this order, as the `par_fns` they replaced did.
        let checks: [&dyn Fn(); 3] = [
            &|| {
                sess.time("looking_for_entry_point", || tcx.ensure_ok().entry_fn(()));
                sess.time("check_externally_implementable_items", || {
                    tcx.ensure_ok().check_externally_implementable_items(())
                });

                sess.time("looking_for_derive_registrar", || {
                    tcx.ensure_ok().proc_macro_decls_static(())
                });

                CStore::from_tcx(tcx).report_unused_deps(tcx);
            },
            &|| {
                tcx.ensure_ok().exportable_items(LOCAL_CRATE);
                tcx.ensure_ok().stable_order_of_exportable_impls(LOCAL_CRATE);
                // Both per-module queries are themselves stages over the module's item-likes
                // (`rustc_passes::item_likes`), so a crate of one module still spreads.
                let modules = tcx.hir_module_ids();
                run_stage(modules, modules.len(), |modules, index| {
                    let module = modules[index];
                    tcx.ensure_ok().check_mod_attrs(module);
                    tcx.ensure_ok().check_mod_unstable_api_usage(module);
                });
            },
            &|| {
                // We force these queries to run,
                // since they might not otherwise get called.
                // This marks the corresponding crate-level attributes
                // as used, and ensures that their values are valid.
                tcx.ensure_ok().limits(());
            },
        ];
        run_stage(&checks, checks.len(), |checks, index| checks[index]());
    });

    sess.time("emit_ast_lowering_delayed_lints", || {
        emit_delayed_lints(tcx);
    });

    crate::rustc_hir_analysis::check_crate(tcx);
    // Freeze definitions as we don't add new ones at this point.
    // We need to wait until now since we synthesize a by-move body
    // for all coroutine-closures.
    //
    // This improves performance by allowing lock-free access to them.
    tcx.untracked().definitions.freeze();

    sess.time("MIR_borrow_checking", || {
        let owners = tcx.hir_body_owner_ids();
        run_stage(owners, owners.len(), |owners, index| {
            let def_id = owners[index];
            let not_typeck_child = !tcx.is_typeck_child(def_id.to_def_id());
            if not_typeck_child {
                // Child unsafety and borrowck happens together with the parent
                tcx.ensure_ok().check_unsafety(def_id);
            }
            if tcx.is_trivial_const(def_id) {
                return;
            }
            if not_typeck_child {
                tcx.ensure_ok().mir_borrowck(def_id);
                tcx.ensure_ok().check_transmutes(def_id);
                if !tcx.sess.opts.unstable_opts.offload.is_empty() {
                    tcx.ensure_ok().check_offloads(def_id);
                }
            }
            tcx.ensure_ok().has_ffi_unwind_calls(def_id);
            tcx.ensure_ok().check_liveness(def_id);

            // If we need to codegen, ensure that we emit all errors from
            // `mir_drops_elaborated_and_const_checked` now, to avoid discovering
            // them later during codegen.
            if tcx.sess.opts.output_types.should_codegen()
                || tcx.hir_body_const_context(def_id).is_some()
            {
                tcx.ensure_ok().mir_drops_elaborated_and_const_checked(def_id);
            }
            if tcx.is_coroutine(def_id.to_def_id())
                && (!tcx.is_async_drop_in_place_coroutine(def_id.to_def_id()))
            {
                // Eagerly check the unsubstituted layout for cycles.
                tcx.ensure_ok()
                    .layout_of(ty::TypingEnv::codegen(tcx, def_id.to_def_id()).as_query_input(
                        tcx.type_of(def_id).instantiate_identity().skip_norm_wip(),
                    ));
            }
        });
    });

    sess.time("layout_testing", || layout_test::test_layout(tcx));
    sess.time("abi_testing", || abi_test::test_abi(tcx));
}

/// Runs the type-checking, region checking and other miscellaneous analysis
/// passes on the crate.
fn analysis(tcx: TyCtxt<'_>, (): ()) {
    run_required_analyses(tcx);

    let sess = tcx.sess;

    // Avoid overwhelming user with errors if borrow checking failed.
    // I'm not sure how helpful this is, to be honest, but it avoids a
    // lot of annoying errors in the ui tests (basically,
    // lint warnings and so on -- kindck used to do this abort, but
    // kindck is gone now). -nmatsakis
    //
    // But we exclude lint errors from this, because lint errors are typically
    // less serious and we're more likely to want to continue (#87337).
    if let Some(guar) = sess.dcx().has_errors_excluding_lint_errors() {
        guar.raise_fatal();
    }

    sess.time("misc_checking_3", || {
        // Two independent groups as one stage over the groups themselves, the first holding a
        // nested stage of four checks, each of the module-wide ones a stage over the crate's
        // modules. Serially they run in exactly this order, as the nested `par_fns` they replaced
        // did; in parallel a group's inner stages run their own items rather than waiting on the
        // pool. Every per-module query below is in turn a stage over the module's owners (its
        // item-likes, or for the late lints its top-level items), so a crate of one module still
        // spreads; see `research/per-owner-passes.md`.
        let per_module = |check: &dyn Fn(LocalModId)| {
            let modules = tcx.hir_module_ids();
            run_stage(modules, modules.len(), |modules, index| check(modules[index]));
        };
        let groups: [&dyn Fn(); 2] = [
            &|| {
                tcx.ensure_ok().effective_visibilities(());

                let checks: [&dyn Fn(); 4] = [
                    &|| per_module(&|module| tcx.ensure_ok().check_private_in_public(module)),
                    &|| per_module(&|module| tcx.ensure_ok().check_mod_deathness(module)),
                    &|| {
                        // **Skipped when every lint is capped to `Allow`, which is the consumer's
                        // default.** `check_crate` walks the whole HIR and runs every late and
                        // per-module lint pass, and it never consults the cap: capping changes
                        // what may be *emitted*, not whether the work happens. So a compile that
                        // has already decided it will print no lint was paying for the entire
                        // walk and discarding the result.
                        //
                        // The condition is exact rather than a heuristic. `lint_cap` is the
                        // ceiling for every lint in the crate, so `Allow` means no lint can fire
                        // at any level from any source - a `#[deny]` in the source included - and
                        // the pass is provably incapable of producing output.
                        //
                        // The consumer sets the cap and so owns this decision; its own flag
                        // raises it to `Warn`, which runs the passes and reports them while still
                        // preventing a source file's own `#![deny(warnings)]` from aborting a
                        // compile we only wanted to analyse.
                        if sess.opts.lint_cap != Some(crate::rustc_lint_defs::Level::Allow) {
                            sess.time("lint_checking", || {
                                crate::rustc_lint::check_crate(tcx);
                            });
                        }
                    },
                    &|| {
                        tcx.ensure_ok().clashing_extern_declarations(());
                    },
                ];
                run_stage(&checks, checks.len(), |checks, index| checks[index]());
            },
            &|| {
                sess.time("privacy_checking_modules", || {
                    per_module(&|module| tcx.ensure_ok().check_mod_privacy(module));
                });
            },
        ];
        run_stage(&groups, groups.len(), |groups, index| groups[index]());

        // This check has to be run after all lints are done processing. We don't
        // define a lint filter, as all lint checks should have finished at this point.
        sess.time("check_lint_expectations", || tcx.ensure_ok().check_expectations(None));

        // This query is only invoked normally if a diagnostic is emitted that needs any
        // diagnostic item. If the crate compiles without checking any diagnostic items,
        // we will fail to emit overlap diagnostics. Thus we invoke it here unconditionally.
        let _ = tcx.all_diagnostic_items(());

        // This query is only invoked normally if a diagnostic is emitted that needs any
        // canonical symbol. If the crate compiles without checking any runtime symbols,
        // we will fail to emit overlap diagnostics. Thus we invoke it here unconditionally.
        let _ = tcx.all_canonical_symbols(());
    });

    // If `-Zvalidate-mir` is set, we also want to compute the final MIR for each item
    // (either its `mir_for_ctfe` or `optimized_mir`) since that helps uncover any bugs
    // in MIR optimizations that may only be reachable through codegen, or other codepaths
    // that requires the optimized/ctfe MIR, coroutine bodies, or evaluating consts.
    // Nevertheless, wait after type checking is finished, as optimizing code that does not
    // type-check is very prone to ICEs.
    if tcx.sess.opts.unstable_opts.validate_mir {
        sess.time("ensuring_final_MIR_is_computable", || {
            let owners = tcx.hir_body_owner_ids();
            run_stage(owners, owners.len(), |owners, index| {
                let def_id = owners[index];
                if !tcx.is_trivial_const(def_id) {
                    tcx.instance_mir(ty::InstanceKind::Item(def_id.into()));
                }
            });
        });
    }
}

/// Compute and validate the crate name.
pub fn get_crate_name(sess: &Session, krate_attrs: &[ast::Attribute]) -> Symbol {
    // We validate *all* occurrences of `#![crate_name]`, pick the first find and
    // if a crate name was passed on the command line via `--crate-name` we enforce
    // that they match.
    // We perform the validation step here instead of later to ensure it gets run
    // in all code paths that require the crate name very early on, namely before
    // macro expansion.

    let attr_crate_name =
        parse_crate_name(sess, krate_attrs, ShouldEmit::EarlyFatal { also_emit_lints: true });

    let validate = |name, span| {
        crate::rustc_session::output::validate_crate_name(sess, name, span);
        name
    };

    if let Some(crate_name) = &sess.opts.crate_name {
        let crate_name = Symbol::intern(crate_name);
        if let Some((attr_crate_name, span)) = attr_crate_name
            && attr_crate_name != crate_name
        {
            sess.dcx().emit_err(diagnostics::CrateNameDoesNotMatch {
                span,
                crate_name,
                attr_crate_name,
            });
        }
        return validate(crate_name, None);
    }

    if let Some((crate_name, span)) = attr_crate_name {
        return validate(crate_name, Some(span));
    }

    if let Input::File(ref path) = sess.io.input
        && let Some(file_stem) = path.file_stem().and_then(|s| s.to_str())
    {
        if file_stem.starts_with('-') {
            sess.dcx().emit_err(diagnostics::CrateNameInvalid { crate_name: file_stem });
        } else {
            return validate(Symbol::intern(&file_stem.replace('-', "_")), None);
        }
    }

    sym::rust_out
}

pub(crate) fn parse_crate_name(
    sess: &Session,
    attrs: &[ast::Attribute],
    emit_errors: ShouldEmit,
) -> Option<(Symbol, Span)> {
    let crate::rustc_hir::Attribute::Parsed(AttributeKind::CrateName { name, name_span, .. }) =
        AttributeParser::parse_limited_sym_should_emit(
            sess,
            attrs,
            &[sym::crate_name],
            DUMMY_SP,
            None,
            emit_errors,
        )?
    else {
        unreachable!("crate_name is the only attr we could've parsed here");
    };

    Some((name, name_span))
}

pub fn collect_crate_types(
    session: &Session,
    supported_crate_types: &[CrateType],
    supported_by: &'static str,
    attrs: &[ast::Attribute],
    crate_span: Span,
) -> Vec<CrateType> {
    // If we're generating a test executable, then ignore all other output
    // styles at all other locations
    if session.opts.test {
        if !session.target.executables {
            session.dcx().emit_warn(diagnostics::UnsupportedCrateTypeForTarget {
                crate_type: CrateType::Executable,
                target_triple: &session.opts.target_triple,
            });
            return Vec::new();
        }
        return vec![CrateType::Executable];
    }

    // Shadow `sdylib` crate type in interface build.
    if session.opts.unstable_opts.build_sdylib_interface {
        return vec![CrateType::Rlib];
    }

    // Only check command line flags if present. If no types are specified by
    // command line, then reuse the empty `base` Vec to hold the types that
    // will be found in crate attributes.
    // JUSTIFICATION: before wrapper fn is available
    let mut base = session.opts.crate_types.clone();
    if base.is_empty() {
        if let Some(Attribute::Parsed(AttributeKind::CrateType(crate_type))) =
            AttributeParser::parse_limited_sym_should_emit(
                session,
                attrs,
                &[sym::crate_type],
                crate_span,
                None,
                ShouldEmit::EarlyFatal { also_emit_lints: false },
            )
        {
            base.extend(crate_type);
        }

        if base.is_empty() {
            base.push(default_output_for_target(session));
        } else {
            base.sort();
            base.dedup();
        }
    }

    base.retain(|crate_type| {
        if invalid_output_for_target(session, *crate_type) {
            session.dcx().emit_warn(diagnostics::UnsupportedCrateTypeForTarget {
                crate_type: *crate_type,
                target_triple: &session.opts.target_triple,
            });
            false
        } else if !supported_crate_types.contains(crate_type) {
            session.dcx().emit_warn(diagnostics::UnsupportedCrateTypeForFrontend {
                crate_type: *crate_type,
                supported_by,
            });
            false
        } else {
            true
        }
    });

    base
}

/// Returns default crate type for target
///
/// Default crate type is used when crate type isn't provided neither
/// through cmd line arguments nor through crate attributes
///
/// It is CrateType::Executable for all platforms but iOS as there is no
/// way to run iOS binaries anyway without jailbreaking and
/// interaction with Rust code through static library is the only
/// option for now
fn default_output_for_target(sess: &Session) -> CrateType {
    if !sess.target.executables { CrateType::StaticLib } else { CrateType::Executable }
}

fn get_recursion_limit(krate_attrs: &[ast::Attribute], sess: &Session) -> Limit {
    let attr = AttributeParser::parse_limited_sym_should_emit(
        sess,
        &krate_attrs,
        &[sym::recursion_limit],
        DUMMY_SP,
        None,
        // errors are fatal here, but lints aren't.
        // If things aren't fatal we continue, and will parse this again.
        // That makes the same lint trigger again.
        // So, no lints here to avoid duplicates.
        ShouldEmit::EarlyFatal { also_emit_lints: false },
    );
    crate::rustc_interface::limits::get_recursion_limit(attr.as_slice(), sess)
}
