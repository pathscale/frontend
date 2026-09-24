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
//! This is a `std` program, so it is the party that can catch a panic: it installs
//! `std::panic::catch_unwind` as frontend's catcher before anything else, which is what lets a
//! refused program come back as an answer instead of ending the process.

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn main() {
    frontend::unwind_janky::install_catcher(catcher);
    let mut args = std::env::args().skip(1).peekable();
    let mut check = false;
    let mut root: Option<String> = None;
    let mut target: Option<String> = None;
    let mut edition: Option<String> = None;
    let mut items_only = false;
    while let Some(flag) = args.peek().filter(|arg| arg.starts_with("--")).cloned() {
        args.next();
        let mut value = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("frontend-facts: {name} needs a value");
                std::process::exit(2);
            })
        };
        match flag.as_str() {
            "--check" => check = true,
            "--root" => root = Some(value("--root")),
            "--target" => target = Some(value("--target")),
            "--edition" => edition = Some(value("--edition")),
            "--items" => items_only = true,
            other => {
                eprintln!("frontend-facts: unknown flag {other}");
                std::process::exit(2);
            }
        }
    }
    let crate_name = args.next().unwrap_or_else(|| "crate".to_string());
    if let Some(root) = root {
        if check {
            eprintln!("frontend-facts: --check reads stdin, not --root");
            std::process::exit(2);
        }
        let facts = frontend::frontend_facts::analyze_crate(
            &crate_name,
            eko::path::Path::new(&root),
            None,
            target.as_deref(),
            edition.as_deref(),
            items_only,
            1,
        );
        match facts {
            Ok(facts) => print_or_exit(serde_json::to_string(&facts)),
            Err(refused) => {
                for diagnostic in &refused.diagnostics {
                    eprintln!("{diagnostic}");
                }
                std::process::exit(1);
            }
        }
        return;
    }
    let mut source = String::new();
    if let Err(error) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut source) {
        eprintln!("frontend-facts: read stdin: {error}");
        std::process::exit(2);
    }
    let json = if check {
        let checked = frontend::frontend_facts::check_source(&crate_name, &source);
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
