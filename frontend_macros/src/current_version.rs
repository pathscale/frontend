use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;

pub(crate) fn current_version(_input: TokenStream) -> TokenStream {
    let env_var = "CFG_RELEASE";
    TokenStream::from(match RustcVersion::parse_cfg_release(env_var) {
        Ok(RustcVersion { major, minor, patch }) => quote!(
            // The produced literal has type `frontend::rustc_session::RustcVersion`.
            Self { major: #major, minor: #minor, patch: #patch }
        ),
        Err(err) => syn::Error::new(Span::call_site(), format!("{env_var} env var: {err}"))
            .into_compile_error(),
    })
}

struct RustcVersion {
    major: u16,
    minor: u16,
    patch: u16,
}

impl RustcVersion {
    /// The version to report when `CFG_RELEASE` is not set.
    ///
    /// Upstream's bootstrap always sets it, so upstream can treat its absence as an error. This
    /// fork has no bootstrap and is consumed as an ordinary cargo dependency, where the variable
    /// arrives only if the consumer knows to set it - and the failure, when they do not, is a
    /// compile error inside a macro expansion in a crate they never named.
    ///
    /// Defaulting here rather than in a build script covers every user of the macro at once. A
    /// build script has to be added per crate, and a crate that reaches `CFG_RELEASE` through this
    /// macro rather than through `env!` is invisible to a grep for the variable, which is exactly
    /// how `rustc_ast` was missed.
    ///
    /// Keep this in step with `CFG_RELEASE` in `.cargo/config.toml` and in
    /// `compiler/rustc_span/build.rs`.
    const DEFAULT_RELEASE: &'static str = "1.100.0-dev";

    fn parse_cfg_release(env_var: &str) -> Result<Self, Box<dyn std::error::Error>> {
        // `std::env::var`, not `proc_macro::tracked::env_var`: the tracked form is nightly-only.
        // The cost is that cargo is not told this variable feeds the expansion, so changing
        // `CFG_RELEASE` alone does not rebuild the crates that use this macro; a crate that
        // cares declares `cargo:rerun-if-env-changed=CFG_RELEASE` in its own build script.
        let value =
            std::env::var(env_var).unwrap_or_else(|_| Self::DEFAULT_RELEASE.to_string());

        Self::parse_str(&value)
            .ok_or_else(|| format!("failed to parse rustc version: {:?}", value).into())
    }

    fn parse_str(value: &str) -> Option<Self> {
        // Ignore any suffixes such as "-dev" or "-nightly".
        let mut components = value.split('-').next().unwrap().splitn(3, '.');
        let major = components.next()?.parse().ok()?;
        let minor = components.next()?.parse().ok()?;
        let patch = components.next().unwrap_or("0").parse().ok()?;
        Some(RustcVersion { major, minor, patch })
    }
}
