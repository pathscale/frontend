// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::borrow::Cow;

use crate::rustc_error_messages::{
    DiagArgMap, DiagArgName, IntoDiagArg, render_message, try_render_message,
};
use tracing::trace;

use crate::rustc_errors::{DiagMessage, Style};

/// Convert `DiagMessage`s to a string
pub fn format_diag_messages(
    messages: &[(DiagMessage, Style)],
    args: &DiagArgMap,
) -> Cow<'static, str> {
    Cow::Owned(messages.iter().map(|(m, _)| format_diag_message(m, args)).collect::<String>())
}

/// Convert a `DiagMessage` to a string
pub fn format_diag_message<'a>(message: &'a DiagMessage, args: &DiagArgMap) -> Cow<'a, str> {
    match message {
        DiagMessage::Str(msg) => Cow::Borrowed(msg),
        DiagMessage::Inline(msg) => format_template(msg, args),
    }
}

/// Substitute a template's `{$args}`.
///
/// This used to stand up a whole `FluentBundle` per message: parse the template into a
/// single-message resource, register the `STREQ` function, convert the argument map into
/// `FluentArgs`, format, and panic if the resolver reported anything. `render_message` does the
/// same parse and the same substitution without the i18n stack behind it, and panics on the same
/// two conditions.
fn format_template(message: &str, args: &DiagArgMap) -> Cow<'static, str> {
    trace!(?message, ?args);
    render_message(message, args)
}

/// The same substitution as `format_diag_message`, for an emitter that must not panic.
///
/// `None` means the message is a template that could not be resolved against these arguments,
/// and the caller is holding the only copy of what went wrong. `DiagMessage::Str` is already
/// final text, so it never fails: only a template can.
pub fn try_format_diag_message<'a>(
    message: &'a DiagMessage,
    args: &DiagArgMap,
) -> Option<Cow<'a, str>> {
    match message {
        DiagMessage::Str(msg) => Some(Cow::Borrowed(msg)),
        DiagMessage::Inline(msg) => try_render_message(msg, args).map(Cow::Owned),
    }
}

/// The text of a message with no substitution attempted: final text, or the raw template.
///
/// This is what `PlainEmitter` and a consumer-supplied sink print when
/// `try_format_diag_message` says no. It exists because `DiagMessage::as_str` answers `None`
/// for a template, and an emitter that filters those out drops the message entirely - which is
/// how `error[E0463]:` with nothing after the colon reached a terminal.
pub fn diag_message_source<'a>(message: &'a DiagMessage) -> &'a str {
    match message {
        DiagMessage::Str(msg) | DiagMessage::Inline(msg) => msg,
    }
}

pub trait DiagMessageAddArg {
    fn arg(self, name: impl Into<DiagArgName>, arg: impl IntoDiagArg) -> EagerDiagMessageBuilder;
}

pub struct EagerDiagMessageBuilder {
    template: Cow<'static, str>,
    args: DiagArgMap,
}

impl DiagMessageAddArg for EagerDiagMessageBuilder {
    fn arg(
        mut self,
        name: impl Into<DiagArgName>,
        arg: impl IntoDiagArg,
    ) -> EagerDiagMessageBuilder {
        let name = name.into();
        let value = arg.into_diag_arg(&mut None);
        debug_assert!(
            !self.args.contains_key(&name) || self.args.get(&name) == Some(&value),
            "arg {} already exists",
            name
        );
        self.args.insert(name, value);
        self
    }
}

impl DiagMessageAddArg for DiagMessage {
    fn arg(self, name: impl Into<DiagArgName>, arg: impl IntoDiagArg) -> EagerDiagMessageBuilder {
        let DiagMessage::Inline(template) = self else {
            panic!("Tried to eagerly format an already formatted message")
        };
        EagerDiagMessageBuilder { template, args: Default::default() }.arg(name, arg)
    }
}

impl EagerDiagMessageBuilder {
    pub fn format(self) -> DiagMessage {
        DiagMessage::Str(format_template(&self.template, &self.args))
    }
}
