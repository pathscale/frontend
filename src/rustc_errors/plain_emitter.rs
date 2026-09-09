//! Diagnostics as lines of text, with no terminal in them.
//!
//! This replaces `AnnotateSnippetEmitter`, which was the only human-readable emitter here and
//! drew the thing you see from `rustc` on a terminal: the source line, carets under the primary
//! span, colour, unicode box drawing, a width negotiated with the tty.
//!
//! **There is no terminal.** This compiler is driven by a program, and a consumer-supplied sink replaces the
//! session's emitter with one that collects diagnostics as structured records and sends them over
//! the wire. The snippet renderer was reachable only through an `ErrorOutputType::HumanReadable`
//! that nothing selects, and it was pulling in `annotate-snippets`, `anstream`, `anstyle` and
//! `termize` to do it.
//!
//! What is left is what a log line needs: severity, code, location, message. A client that wants
//! to draw a snippet has the span and the source and can do it where a terminal exists.

use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::rustc_span::source_map::SourceMap;
use alloc::sync::Arc;

use crate::rustc_errors::emitter::Emitter;
use crate::rustc_errors::formatting::{diag_message_source, try_format_diag_message};
use crate::rustc_errors::{DiagArgValue, DiagInner, Level};

/// Writes each diagnostic as one or more plain lines to stderr.
pub struct PlainEmitter {
    sm: Option<Arc<SourceMap>>,
    short: bool,
    // Where the lines go. `None` is stderr, which is every real use; a destination is how the
    // parser's own tests capture what was emitted, which they previously did by handing the
    // snippet renderer an `anstream` buffer.
    dst: Option<alloc::boxed::Box<dyn core::fmt::Write + Send>>,
}

impl PlainEmitter {
    /// An emitter with no source map, so diagnostics carry no location.
    pub fn new() -> PlainEmitter {
        PlainEmitter { sm: None, short: false, dst: None }
    }

    /// Attach a source map, so a primary span becomes `file:line:col`.
    pub fn sm(mut self, sm: Option<Arc<SourceMap>>) -> PlainEmitter {
        self.sm = sm;
        self
    }

    /// One line per diagnostic: drop the sub-diagnostics.
    pub fn short_message(mut self, short: bool) -> PlainEmitter {
        self.short = short;
        self
    }

    /// Send the lines somewhere other than stderr.
    pub fn dst(mut self, dst: alloc::boxed::Box<dyn core::fmt::Write + Send>) -> PlainEmitter {
        self.dst = Some(dst);
        self
    }
}

impl Default for PlainEmitter {
    fn default() -> PlainEmitter {
        PlainEmitter::new()
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Bug | Level::DelayedBug => "internal compiler error",
        Level::Fatal | Level::Error => "error",
        Level::ForceWarning | Level::Warning => "warning",
        Level::Note | Level::OnceNote => "note",
        Level::Help | Level::OnceHelp => "help",
        Level::FailureNote => "note",
        Level::Allow => "allow",
        Level::Expect => "expect",
    }
}

impl Emitter for PlainEmitter {
    fn emit_diagnostic(&mut self, diag: DiagInner) {
        let mut out = String::new();
        let _ = write!(out, "{}", level_name(diag.level()));
        if let Some(code) = diag.code {
            let _ = write!(out, "[{code}]");
        }

        // Substitute the template's `{$args}`.
        //
        // This used to say the renderer lived in the snippet emitter and print the template
        // instead. That was wrong twice over once `frontend_diag_template` replaced Fluent:
        // the renderer is `crate::rustc_errors::formatting`, one module over, and `DiagMessage::as_str`
        // answers `None` for a template, so `filter_map` did not even print it - a derived
        // diagnostic came out as `error[E0463]:` with nothing after the colon.
        //
        // `unresolved` records whether any message had to fall back, and it is what turns the
        // argument dump below from the normal case into the fallback it was meant to be.
        let mut unresolved = false;
        let message: Vec<String> = diag
            .messages
            .iter()
            .map(|(m, _)| {
                let text = match try_format_diag_message(m, &diag.args) {
                    Some(text) => text,
                    None => {
                        unresolved = true;
                        Cow::Borrowed(diag_message_source(m))
                    }
                };
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            })
            .collect();
        let _ = write!(out, ": {}", message.join(" "));

        if let Some(sm) = &self.sm
            && let Some(sp) = diag.span.primary_span()
            && !sp.is_dummy()
        {
            let _ = write!(out, "\n  --> {}", sm.span_to_diagnostic_string(sp));
        }

        // The fallback, and only the fallback. When every message rendered, its arguments are
        // already in the prose above and repeating them is noise; when one did not, this is the
        // information the reader would otherwise lose, so it is still printed.
        if !self.short && unresolved {
            for (name, value) in diag.args.iter() {
                let rendered = match value {
                    DiagArgValue::Str(s) => s.to_string(),
                    DiagArgValue::Number(n) => n.to_string(),
                    DiagArgValue::StrListSepByAnd(xs) => {
                        xs.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")
                    }
                };
                let _ = write!(out, "\n  {name} = {rendered}");
            }
        }

        out.push('\n');
        match self.dst.as_mut() {
            Some(w) => {
                let _ = w.write_str(&out);
            }
            None => eko::print::write_fd(2, out.as_bytes()),
        }
    }

    fn source_map(&self) -> Option<&SourceMap> {
        self.sm.as_deref()
    }
}
