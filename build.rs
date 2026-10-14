use std::env;

// rustc's crates read these with `env!`/`option_env!` at compile time. In this checkout
// `.cargo/config.toml` sets them, but that file does not ship in the published package, so a
// crate depending on frontend from a registry would get none of them and `env!("CFG_RELEASE")`
// would not compile. Emit each one here, taking the caller's value when set and otherwise the
// same default this checkout's config uses, so a dependency build behaves like the local one.
const DEFAULTS: &[(&str, &str)] = &[
    ("CFG_RELEASE", "1.100.0-dev"),
    ("CFG_VERSION", "1.100.0-dev"),
    ("CFG_RELEASE_CHANNEL", "nightly"),
    ("RUSTC_INSTALL_BINDIR", "bin"),
];

fn main() {
    for (name, default) in DEFAULTS {
        println!("cargo::rerun-if-env-changed={name}");
        let value = env::var(name).unwrap_or_else(|_| (*default).to_string());
        println!("cargo::rustc-env={name}={value}");
    }
    println!("cargo::rerun-if-env-changed=CFG_COMPILER_HOST_TRIPLE");
    let host_triple = env::var("CFG_COMPILER_HOST_TRIPLE")
        .or_else(|_| env::var("TARGET"))
        .unwrap_or_default();
    println!("cargo::rustc-env=CFG_COMPILER_HOST_TRIPLE={host_triple}");
}
