//! Read a crate from stdin and report on it, as JSON on stdout.
//!
//! `frontend-facts CRATE` emits [`frontend::frontend_facts::CrateFacts`].
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
    let check = args.peek().is_some_and(|a| a == "--check");
    if check {
        args.next();
    }
    let crate_name = args.next().unwrap_or_else(|| "crate".to_string());
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
