//! This crate used to configure rust logging (`-Zlog` / the `RUSTC_LOG` family of environment
//! variables) without a tool having to magically match rustc's tracing crate version.
//!
//! # What this crate no longer does, and why
//!
//! **The logging sink is gone.** `init_logger` is a no-op that reports `-Zlog` is unsupported.
//!
//! The implementation was built on `tracing_subscriber`: an `EnvFilter` for the directive
//! string, a `tracing_tree::HierarchicalLayer` or a JSON `fmt` layer for the output, and a
//! `Registry` underneath them. `std` is banned in this tree, and none of those three survive
//! that. In `tracing-subscriber` 0.3 the cargo features are wired
//!
//! ```text
//! fmt        = ["registry", "std"]
//! env-filter = [..., "std", ...]
//! registry   = ["sharded-slab", "thread_local", "std"]
//! ```
//!
//! so every piece this crate used turns `std` on, and cargo features are additive - there is no
//! subset that leaves it off. The writer is the same story one level down:
//! `tracing_subscriber::fmt::MakeWriter` declares `type Writer: std::io::Write`, so any writer
//! handed to a `fmt` layer must implement that trait. Routing the bytes through
//! `eko::file::File` / `eko::print::write_fd` was tried first and does not
//! help, because the adapter that makes those look like a `std::io::Write` is itself the `std`
//! dependency. Hence the whole subscriber stack is removed rather than rewired.
//!
//! Removed with it:
//!
//! - `init_logger_with_additional_layer` and the `BuildSubscriberRet` trait alias. Both are
//!   spelled in terms of `tracing_subscriber::registry::LookupSpan`, which no longer exists here.
//! - the `tracing_subscriber` re-export.
//! - the `RUSTC_LOG_BACKTRACE` layer. It had already lost its capture (there is no backtracer
//!   without `std`) and announced the event only.
//! - the recursion filter for the JSON format, which existed to stop a `fmt` layer logging the
//!   formatting of its own arguments.
//!
//! `LoggerConfig`, `init_logger`, `Error`, `stdout_isatty` and `stderr_isatty` keep their
//! signatures so callers still compile. `LoggerConfig::from_env` still reads the environment and
//! `init_logger` still rejects an invalid `_COLOR` or `_WRAPTREE` value, so a malformed
//! invocation is still an error rather than silence - it is only the output that is gone.
//!
//! Restoring `-Zlog` means writing a `tracing_core::Subscriber` directly against `tracing-core`
//! (which does have a `no_std` path) plus a directive parser, and registering it with
//! `tracing::subscriber::set_global_default`. That is a new implementation, not a re-wiring.

// `#![no_std]`: the prelude items arrive with no path, so a `std::` search cannot see them - and
// a `#[derive]` can use them without the name appearing in this file at all, which is why they
// are not trimmed by inspection.

// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// `#![no_std]` above is the ban and the compiler is the enforcer: without `extern crate std;`
// there is no `std` in the extern prelude, so any `std::` path fails to resolve and the build
// stops. Do not add that line back to make an error go away - the error is the point. Whatever
// needed `std` either has a `core`/`alloc` equivalent, belongs in `ekostd`, or is a
// dependency that has to be replaced. `tracing_subscriber` was the third case: see the module
// documentation above.
//
// The prelude is the part a grep cannot see: `Vec`, `String`, `Box`, `format!`, `vec!`,
// `thread_local!` and `println!` name no path. Under `#![no_std]` they resolve through `alloc`
// and `eko` instead, which is why those imports appear at the top of every file here.
// ---------------------------------------------------------------------------------------------
use alloc::format;
use alloc::string::String;

use core::fmt::{self, Display};

use eko::env;
use tracing::dispatcher::SetGlobalDefaultError;

// Re-export tracing. `tracing_subscriber` was re-exported here too and is no longer a dependency.
//
// `::tracing`, not `tracing`. `lib.rs` carries `#[macro_use] extern crate tracing;`, which binds
// the name at the crate root as a *private* item, and re-exporting a private extern crate is
// E0365. Naming it through the extern prelude re-exports the crate rather than that binding.
pub use {::tracing, ::tracing_core};

/// The values of all the environment variables that matter for configuring a logger.
///
/// Behaviour change: these were `Result<String, VarError>`. `eko::env::var` returns
/// `Option<String>`, folding "unset" and "set to non-UTF-8" into `None`, so the two cases can no
/// longer be told apart here.
pub struct LoggerConfig {
    pub filter: Option<String>,
    pub color_logs: Option<String>,
    pub verbose_entry_exit: Option<String>,
    pub verbose_thread_ids: Option<String>,
    pub backtrace: Option<String>,
    pub json: Option<String>,
    pub output_target: Option<String>,
    pub wraptree: Option<String>,
    pub lines: Option<String>,
}

impl LoggerConfig {
    pub fn from_env(env: &str) -> Self {
        // NOTE: documented in the dev guide. If you change this, also update it!
        LoggerConfig {
            filter: env::var(env),
            color_logs: env::var(&format!("{env}_COLOR")),
            verbose_entry_exit: env::var(&format!("{env}_ENTRY_EXIT")),
            verbose_thread_ids: env::var(&format!("{env}_THREAD_IDS")),
            backtrace: env::var(&format!("{env}_BACKTRACE")),
            wraptree: env::var(&format!("{env}_WRAPTREE")),
            lines: env::var(&format!("{env}_LINES")),
            json: env::var(&format!("{env}_FORMAT_JSON")),
            output_target: env::var(&format!("{env}_OUTPUT_TARGET")),
        }
    }

    /// Whether anything in the environment actually asked for logging.
    ///
    /// Used to decide whether the "unsupported" notice is worth printing: an invocation that set
    /// none of these did not ask for logs and must not be made noisy by their absence.
    fn requested(&self) -> bool {
        self.filter.is_some()
            || self.color_logs.is_some()
            || self.verbose_entry_exit.is_some()
            || self.verbose_thread_ids.is_some()
            || self.backtrace.is_some()
            || self.json.is_some()
            || self.output_target.is_some()
            || self.wraptree.is_some()
            || self.lines.is_some()
    }
}

/// Initialize the logger with the given values for the filter, coloring, and other options env
/// variables.
///
/// **Behaviour change: this no longer installs a logger.** It validates the configuration and,
/// if the environment asked for logging at all, writes one line to stderr saying `-Zlog` is
/// unsupported in this build. See the module documentation for why the subscriber was removed.
pub fn init_logger(cfg: LoggerConfig) -> Result<(), Error> {
    // The configuration is still checked, so an invalid value is still an error rather than
    // being silently accepted by a function that ignores it.
    match &cfg.color_logs {
        Some(value) => match value.as_ref() {
            // Behaviour change: a non-UTF-8 value used to be `Error::NonUnicodeColorValue`.
            // `eko::env::var` reports it as `None`, indistinguishable from unset, so it
            // is no longer reachable.
            "always" | "never" | "auto" => {}
            _ => return Err(Error::InvalidColorValue(value.clone())),
        },
        None => {}
    }

    if let Some(v) = &cfg.wraptree
        && v.parse::<usize>().is_err()
    {
        return Err(Error::InvalidWraptree(v.clone()));
    }

    if cfg.requested() {
        eko::eprintln!(
            "warning: `-Zlog` is unsupported in this build: the tracing_subscriber sink was \
             removed because every feature of it that this compiler used (fmt, env-filter, \
             registry) forces a dependency on std, which is banned in this tree"
        );
    }

    Ok(())
}

pub fn stdout_isatty() -> bool {
    eko::file::stdout().is_terminal()
}

pub fn stderr_isatty() -> bool {
    eko::file::stderr().is_terminal()
}

#[derive(Debug)]
pub enum Error {
    InvalidColorValue(String),
    /// No longer reachable: `eko::env::var` reports a non-UTF-8 value as unset. Kept
    /// because it is part of this crate's public surface.
    NonUnicodeColorValue,
    InvalidWraptree(String),
    /// No longer produced: nothing here calls `set_global_default` any more. Kept, with its
    /// `From` impl, because it is part of this crate's public surface.
    AlreadyInit(SetGlobalDefaultError),
}

impl core::error::Error for Error {}

impl Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidColorValue(value) => write!(
                formatter,
                "invalid log color value '{value}': expected one of always, never, or auto",
            ),
            Error::NonUnicodeColorValue => write!(
                formatter,
                "non-Unicode log color value: expected one of always, never, or auto",
            ),
            Error::InvalidWraptree(value) => write!(
                formatter,
                "invalid log WRAPTREE value '{value}': expected a non-negative integer",
            ),
            Error::AlreadyInit(tracing_error) => Display::fmt(tracing_error, formatter),
        }
    }
}

impl From<SetGlobalDefaultError> for Error {
    fn from(tracing_error: SetGlobalDefaultError) -> Self {
        Error::AlreadyInit(tracing_error)
    }
}
