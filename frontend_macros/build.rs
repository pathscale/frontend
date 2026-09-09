// Upstream refuses here unless `RUSTC_BOOTSTRAP` is set, on the grounds that you are "attempting
// to build the compiler without going through bootstrap". That guard is correct in a tree that has
// a bootstrap. This one does not have one, on purpose: these crates are ordinary cargo
// dependencies and `cargo build` is the whole build.
//
// Keeping the guard would have meant every consumer setting an environment variable it has no way
// to know about, discovering the need through a build script panic in a crate it never named. That
// is not an interface.
//
// The unstable features these crates use are permitted by the nightly `rust-toolchain.toml` pins,
// so nothing here needs `RUSTC_BOOTSTRAP` to compile. Setting it still works and still overrides,
// for anyone building with a stable toolchain on purpose.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
