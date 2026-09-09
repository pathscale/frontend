use syn::{LitStr, parse_macro_input};

use crate::diagnostics::message::{Message, parse_or_report};

pub(crate) fn msg_macro(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let inline = parse_macro_input!(input as LitStr);

    // `msg!` has no struct behind it, so there are no field names to check the template's
    // variables against - they are supplied later by `.arg(..)` calls. The *syntax* still has to
    // be checked here, because the only other place this template is parsed is the emitter, and
    // a parse failure there is a panic while a diagnostic is being printed.
    //
    // Nothing checked it before: the old path parsed a template only for attribute messages, so
    // a malformed `msg!` was a run-time surprise.
    parse_or_report(inline.span(), &inline.value());

    let message =
        Message { attr_span: inline.span(), message_span: inline.span(), value: inline.value() };
    message.diag_message().into()
}
