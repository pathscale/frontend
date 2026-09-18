//! Emit [`frontend::frontend_facts::CrateFacts`] as JSON on stdout.
//!
//! Agentcode cannot path-dep this nightly crate. It spawns this helper instead.
//! Crate name is argv1; source is stdin. A refused program exits 1 without
//! aborting the process.

fn main() {
    let crate_name = std::env::args().nth(1).unwrap_or_else(|| "crate".to_string());
    let mut source = String::new();
    if let Err(error) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut source) {
        eprintln!("frontend-facts: read stdin: {error}");
        std::process::exit(2);
    }
    match frontend::frontend_facts::analyze_source(&crate_name, &source) {
        Ok(facts) => {
            // serde_json is alloc-only in this crate (`default-features = false`).
            let json = match serde_json::to_string(&facts) {
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
        Err(_) => std::process::exit(1),
    }
}
