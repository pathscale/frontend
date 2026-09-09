use std::collections::HashSet;

// `indexmap` is built without `std` here, so the default hasher is gone and the third parameter
// has to be named. See `Cargo.toml`.
type IndexMap<K, V> = indexmap::IndexMap<K, V, rustc_hash::FxBuildHasher>;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use frontend_diag_template::Pattern;
use syn::ext::IdentExt;

use crate::diagnostics::error::span_err;
use crate::diagnostics::utils::FieldMap;

/// Parse a message template, reporting a failure against the literal that carried it.
///
/// **This is the compile-time half of a run-time guarantee.** The emitter parses the same
/// template again when it renders, and it has nowhere to report a syntax error to - a diagnostic
/// being printed is the wrong moment to discover the diagnostic is malformed, so it panics. So
/// every template in the tree passes through here first, whether it came from an attribute or
/// from `msg!`, and an unsupported construct is a build error naming the site.
///
/// It used to be `fluent-bundle` doing the parse, and `.unwrap()` doing the reporting.
pub(crate) fn parse_or_report(message_span: Span, message_str: &str) -> Option<Pattern> {
    match frontend_diag_template::parse(message_str) {
        Ok(pattern) => Some(pattern),
        Err(err) => {
            span_err(
                message_span.unwrap(),
                format!("invalid diagnostic message: {} (at byte {})", err.message, err.offset),
            )
            .emit();
            None
        }
    }
}

#[derive(Clone)]
pub(crate) struct Message {
    pub attr_span: Span,
    pub message_span: Span,
    pub value: String,
}

impl Message {
    pub(crate) fn new(
        attr_span: Span,
        message_span: Span,
        message_str: String,
        field_map: &FieldMap,
        used_fields: &mut HashSet<proc_macro2::Ident>,
    ) -> Self {
        // A message that does not parse has already been reported; there is nothing to check its
        // variables against, and a second error about them would only bury the first.
        if let Some(pattern) = parse_or_report(message_span, &message_str) {
            let mut fields: IndexMap<String, (&syn::Ident, bool)> =
                IndexMap::with_capacity_and_hasher(field_map.len(), Default::default());
            for (_, (ident, _)) in field_map {
                fields.insert(ident.unraw().to_string(), (ident, false));
            }
            for variable in frontend_diag_template::variable_references(&pattern) {
                match fields.get_mut(variable) {
                    Some((_, seen)) => *seen = true,
                    None => {
                        span_err(
                            message_span.unwrap(),
                            format!("Variable `{variable}` not found in diagnostic "),
                        )
                        .help(format!(
                            "Available fields: {:?}",
                            fields.keys().map(|s| s.as_str()).collect::<Vec<&str>>().join(", ")
                        ))
                        .emit();
                    }
                }
            }
            for (name, seen) in fields.values() {
                if *seen {
                    used_fields.insert((*name).clone());
                }
            }
        }
        Self { attr_span, message_span, value: message_str }
    }

    /// Get the diagnostic message for this diagnostic
    /// The passed `variant` is used to check whether all variables in the message are used.
    /// For subdiagnostics, we cannot check this.
    pub(crate) fn diag_message(&self) -> TokenStream {
        let message = &self.value;
        self.verify();
// Kept as an enum *construction*, not a helper call. `Subdiagnostic` expands to
        // `format_diag_message(&<this>, ..)`, and a borrow of a constructor expression
        // const-promotes to `'static` while a borrow of a function call does not - swapping in a
        // helper produced E0716 at every derive site. `Cow` is named through `rustc_errors`
        // because the deriving crate may have neither `alloc` nor `std` in scope.
        quote! { frontend::rustc_errors::DiagMessage::Inline(frontend::rustc_errors::Cow::Borrowed(#message)) }
    }

    fn verify(&self) {
        verify_message_style(self.message_span, &self.value);
        verify_message_formatting(self.attr_span, self.message_span, &self.value);
    }
}

const ALLOWED_CAPITALIZED_WORDS: &[&str] = &[
    // tidy-alphabetical-start
    "ABI",
    "ABIs",
    "ADT",
    "C-variadic",
    "CGU-reuse",
    "Cargo",
    "Ferris",
    "GCC",
    "MIR",
    "NaNs",
    "OK",
    "Rust",
    "ThinLTO",
    "Unicode",
    "VS",
    // tidy-alphabetical-end
];

/// See: https://rustc-dev-guide.rust-lang.org/diagnostics.html#diagnostic-output-style-guide
fn verify_message_style(msg_span: Span, message: &str) {
    // Verify that message starts with lowercase char
    let Some(first_word) = message.split_whitespace().next() else {
        span_err(msg_span.unwrap(), "message must not be empty").emit();
        return;
    };
    let first_char = first_word.chars().next().expect("Word is not empty");
    if first_char.is_uppercase() && !ALLOWED_CAPITALIZED_WORDS.contains(&first_word) {
        span_err(msg_span.unwrap(), "message `{value}` starts with an uppercase letter. Fix it or add it to `ALLOWED_CAPITALIZED_WORDS`").emit();
        return;
    }

    // Verify that message does not end in `.`
    if message.ends_with(".") && !message.ends_with("...") {
        span_err(msg_span.unwrap(), "message `{value}` ends with a period").emit();
        return;
    }
}

/// Verifies that the message is properly indented into the code
fn verify_message_formatting(attr_span: Span, msg_span: Span, message: &str) {
    // Find the indent at the start of the message (`column()` is one-indexed)
    let start = attr_span.unwrap().column() - 1;

    for line in message.lines().skip(1) {
        if line.is_empty() {
            continue;
        }
        let indent = line.chars().take_while(|c| *c == ' ').count();
        if indent < start {
            span_err(
                msg_span.unwrap(),
                format!("message is not properly indented. {indent} < {start}"),
            )
            .emit();
            return;
        }
        if indent % 4 != 0 {
            span_err(msg_span.unwrap(), "message is not indented with a multiple of 4 spaces")
                .emit();
            return;
        }
    }
}
