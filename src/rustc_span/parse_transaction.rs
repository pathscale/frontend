//! Access permissions for an uncommitted parse over already lexed tokens.

struct Access {
    symbols: bool,
    spans: bool,
}

eko::scoped_thread_local!(static ACCESS: Access);

pub(crate) fn file_pass<R>(f: impl FnOnce() -> R) -> R {
    ACCESS.set(&Access { symbols: true, spans: true }, f)
}

pub(crate) fn item_pass<R>(f: impl FnOnce() -> R) -> R {
    ACCESS.set(&Access { symbols: false, spans: true }, f)
}

pub(crate) fn body_pass<R>(f: impl FnOnce() -> R) -> R {
    ACCESS.set(&Access { symbols: false, spans: false }, f)
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
