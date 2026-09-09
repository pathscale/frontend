//! The five variables bootstrap sets, supplied here so that `cargo add frontend` compiles.
//!
//! # Why a build script and not `.cargo/config.toml`
//!
//! This repository has a `.cargo/config.toml` that sets these, and it works when you build *here*.
//! It does nothing for anyone who depends on this crate: cargo reads config from the directory the
//! command was run in and its ancestors, never from a dependency's own directory. So a consumer
//! who runs `cargo add frontend` gets a compile error from `env!("CFG_RELEASE")` inside somebody
//! else's crate, with nothing to say which knob is missing.
//!
//! Two of the reads are `env!` rather than `option_env!` - `CFG_RELEASE` and `CFG_RELEASE_CHANNEL`
//! - so their absence is a hard error rather than a degraded default. Everything else rustc reads
//! is `option_env!` and copes.
//!
//! # These are defaults, and a consumer overrides them
//!
//! Every variable below is passed through unchanged if it is already set, so the local
//! `.cargo/config.toml`, an `[env]` table in a consumer's config, or an exported shell variable
//! all still win. This script only fills in what nobody supplied.
//!
//! # `CFG_VERSION` is not set here on purpose
//!
//! It decides which sysroot the built compiler can read, and it is the one that should not be
//! guessed: an older sysroot desyncs inside a serializer and a newer one fails in type checking,
//! and neither failure names the version. `rustc_interface::Config::rustc_version` sets it per
//! session instead, from the sysroot actually being used:
//!
//! ```ignore
//! config.rustc_version = rustc_interface::util::rustc_version_of_sysroot(&sysroot);
//! ```
//!
//! which asks that sysroot rather than making anyone write the string down twice. It is
//! `option_env!`, so leaving it unset is fine and the runtime value is what counts.

use std::env;

/// Set `key` to `value` unless the environment already has it.
///
/// `cargo::rustc-env` applies to the `rustc` invocation for this crate, which is what makes it
/// able to supply `RUSTC_BOOTSTRAP` as well as the `CFG_*` strings.
fn default_env(key: &str, value: &str) {
    println!("cargo::rerun-if-env-changed={key}");
    let chosen = env::var(key).unwrap_or_else(|_| value.to_string());
    println!("cargo::rustc-env={key}={chosen}");
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    // `RUSTC_BOOTSTRAP` is deliberately not set here, and it was. Cargo refuses it from a build
    // script and says why: "crates cannot set `RUSTC_BOOTSTRAP` themselves, as doing so would
    // subvert the stability guarantees of Rust for your project." It is a warning rather than an
    // error, so the line looked like it worked.
    //
    // Nothing is lost. This crate needs a nightly compiler, not a stable one told to pretend:
    // `rust-toolchain.toml` pins one, and on nightly `#![feature(..)]` needs no such variable.
    // Only a consumer building it on stable or beta needs `RUSTC_BOOTSTRAP=1` in their own
    // environment, and that is their decision to make rather than one this crate makes quietly.

    // The two hard `env!` reads. Everything else rustc looks for is `option_env!`.
    default_env("CFG_RELEASE", "1.100.0-dev");
    default_env("CFG_RELEASE_CHANNEL", "nightly");

    // `option_env!`, but worth a default: it is the target the built compiler reports as its own
    // host, and the host that built it is the right answer whenever nobody says otherwise.
    default_env(
        "CFG_COMPILER_HOST_TRIPLE",
        &env::var("TARGET").unwrap_or_default(),
    );

    // Where a sysroot keeps its binaries. Only the string is used, in a path this compiler
    // reports rather than one it opens.
    default_env("RUSTC_INSTALL_BINDIR", "bin");
}
