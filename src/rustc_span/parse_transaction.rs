//! Access permissions for an uncommitted parse over already lexed tokens.

struct Access {
    symbols: bool,
    spans: bool,
    diagnostics: bool,
}

eko::scoped_thread_local!(static ACCESS: Access);

pub(crate) fn file_pass<R>(f: impl FnOnce() -> R) -> R {
    ACCESS.set(&Access { symbols: true, spans: true, diagnostics: true }, f)
}

pub(crate) fn item_pass<R>(f: impl FnOnce() -> R) -> R {
    ACCESS.set(&Access { symbols: false, spans: true, diagnostics: false }, f)
}

pub(crate) fn body_pass<R>(f: impl FnOnce() -> R) -> R {
    ACCESS.set(&Access { symbols: false, spans: false, diagnostics: false }, f)
}

pub(crate) fn symbol_insert() {
    if ACCESS.is_set() && !ACCESS.with(|access| access.symbols) {
        crate::rustc_data_structures::sync::retry_parse_serial();
    }
}

pub(crate) fn span_insert() {
    if ACCESS.is_set() && !ACCESS.with(|access| access.spans) {
        crate::rustc_data_structures::sync::retry_parse_serial();
    }
}

pub(crate) fn active() -> bool {
    ACCESS.is_set()
}

/// A speculative parser cannot publish diagnostics. Retry before a guarded Diag exists:
/// a later frozen identity insertion may unwind, and dropping that Diag would panic again.
/// Owned file passes retain diagnostics as output and allow their construction.
pub(crate) fn diagnostic() {
    if ACCESS.is_set() && !ACCESS.with(|access| access.diagnostics) {
        crate::rustc_data_structures::sync::retry_parse_serial();
    }
}
