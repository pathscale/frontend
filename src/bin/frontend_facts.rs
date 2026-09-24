//! Read a crate and report on it, as JSON on stdout.
//!
//! `frontend-facts CRATE` reads the crate's one source file from stdin and emits
//! [`frontend::frontend_facts::CrateFacts`]. `frontend-facts --root PATH [--target TUPLE] [--edition YEAR] [--items] CRATE`
//! reads the crate from disk instead, from its root file, with its modules' files where rustc
//! finds them, under `TUPLE`'s `cfg` (the host's by default) and `--edition` (2015 by default,
//! as rustc's). `--items` reads definitions, imports, impls, module names and macros only: no
//! body is type checked and no reference is reported.
//! `frontend-facts --check CRATE` emits [`frontend::frontend_facts::Checked`]: the errors and
//! warnings from type checking, borrow checking and lints. Nothing is compiled to a binary and
//! nothing is run.
//!
//! A crate with dependencies, as rustc's own flags say it:
//!
//! - `--extern NAME=PATH` loads a dependency from the metadata an earlier read wrote
//!   (`noprelude:NAME=PATH` for one only other dependencies name). Repeatable.
//! - `--cfg SPEC` (`feature="std"`) and `--env KEY=VALUE` (what `env!` reads) give the crate the
//!   configuration its manifest and build script would. Repeatable.
//! - `--emit-metadata PATH` writes the crate's metadata for the crates that depend on it, as
//!   `lib<name>.rmeta`; a crate with any error is refused and nothing is written.
//! - `--proc-macro` reads a `proc-macro` crate: its macros are declared, never run.
//! - `--standard-library` reads one of the standard library's crates, or a crate it depends on,
//!   as rustc's bootstrap does (`-Zforce-unstable-if-unmarked`).
//! - `--library` reads the crate as a library its own compiler already compiled, of any version
//!   (`CrateRead::library`): nothing judges it, every error it still meets is in the facts'
//!   `diagnostics`, and `--emit-metadata` writes its metadata anyway.
//!
//! With `--check`, the flags check stdin against the loaded dependencies.
//!
//! This is a `std` program, so it is the party that can catch a panic: it installs
//! `std::panic::catch_unwind` as frontend's catcher before anything else, which is what lets a
//! refused program come back as an answer instead of ending the process.

use frontend::frontend_facts::{CrateRead, Dependency, Loaded};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn usage(message: &str) -> ! {
    eprintln!("frontend-facts: {message}");
    std::process::exit(2);
}

fn main() {
    frontend::unwind_janky::install_catcher(catcher);
    let mut args = std::env::args().skip(1).peekable();
    let mut check = false;
    let mut root: Option<String> = None;
    let mut target: Option<String> = None;
    let mut edition: Option<String> = None;
    let mut items_only = false;
    let mut proc_macro = false;
    let mut standard_library = false;
    let mut library = false;
    let mut emit_metadata: Option<String> = None;
    let mut dependencies: Vec<Dependency> = Vec::new();
    let mut cfg: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    while let Some(flag) = args.peek().filter(|arg| arg.starts_with("--")).cloned() {
        args.next();
        let mut value =
            |name: &str| args.next().unwrap_or_else(|| usage(&format!("{name} needs a value")));
        match flag.as_str() {
            "--check" => check = true,
            "--root" => root = Some(value("--root")),
            "--target" => target = Some(value("--target")),
            "--edition" => edition = Some(value("--edition")),
            "--items" => items_only = true,
            "--proc-macro" => proc_macro = true,
            "--standard-library" => standard_library = true,
            "--library" => library = true,
            "--emit-metadata" => emit_metadata = Some(value("--emit-metadata")),
            "--cfg" => cfg.push(value("--cfg")),
            "--env" => {
                let pair = value("--env");
                let (key, val) =
                    pair.split_once('=').unwrap_or_else(|| usage("--env needs KEY=VALUE"));
                env.push((key.to_string(), val.to_string()));
            }
            "--extern" => {
                let spec = value("--extern");
                let (spec, prelude) = match spec.strip_prefix("noprelude:") {
                    Some(rest) => (rest.to_string(), false),
                    None => (spec, true),
                };
                let (name, path) =
                    spec.split_once('=').unwrap_or_else(|| usage("--extern needs NAME=PATH"));
                let dependency = if prelude {
                    Dependency::new(name, path)
                } else {
                    Dependency::transitive(name, path)
                };
                dependencies.push(dependency);
            }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    let crate_name = args.next().unwrap_or_else(|| "crate".to_string());
    let loaded = Loaded { dependencies: &dependencies, cfg: &cfg, env: &env };
    if let Some(root) = root {
        if check {
            usage("--check reads stdin, not --root");
        }
        let root = eko::path::Path::new(&root);
        let metadata = emit_metadata.as_deref().map(eko::path::Path::new);
        let read = CrateRead {
            target: target.as_deref(),
            edition: edition.as_deref(),
            items_only,
            proc_macro,
            standard_library,
            loaded,
            write_metadata: metadata,
            library,
            ..CrateRead::new(&crate_name, root)
        };
        match frontend::frontend_facts::read_crate(&read) {
            Ok(facts) => print_or_exit(serde_json::to_string(&facts)),
            Err(refused) => {
                eprintln!("frontend-facts: crate `{}` refused", refused.crate_name);
                for diagnostic in &refused.diagnostics {
                    eprintln!("{diagnostic}");
                }
                std::process::exit(1);
            }
        }
        return;
    }
    if emit_metadata.is_some() || proc_macro || library {
        usage("--emit-metadata, --proc-macro and --library read a crate from --root");
    }
    let mut source = String::new();
    if let Err(error) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut source) {
        usage(&format!("read stdin: {error}"));
    }
    let json = if check {
        let checked = frontend::frontend_facts::check_source_against(
            &crate_name,
            std::sync::Arc::new(source),
            edition.as_deref(),
            loaded,
            1,
        );
        let clean = checked.is_clean();
        let json = serde_json::to_string(&checked);
        print_or_exit(json);
        std::process::exit(if clean { 0 } else { 1 });
    } else {
        match frontend::frontend_facts::analyze_source(&crate_name, &source) {
            Ok(facts) => serde_json::to_string(&facts),
            Err(_) => std::process::exit(1),
        }
    };
    print_or_exit(json);
}

fn print_or_exit(json: Result<String, serde_json::Error>) {
    // serde_json is alloc-only in this crate (`default-features = false`).
    let json = match json {
        Ok(json) => json,
        Err(error) => {
            eprintln!("frontend-facts: write json: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = std::io::Write::write_all(&mut std::io::stdout(), json.as_bytes()) {
        eprintln!("frontend-facts: write stdout: {error}");
        std::process::exit(2);
    }
}
