//! Reading one symbol out of a shared object.
//!
//! This exists for exactly one caller - `CrateLoader::dlsym_proc_macros`, which needs the
//! `__rustc_proc_macro_decls_*__` static out of a proc-macro crate's `.dylib`. Upstream reaches
//! it through `libloading`; that crate is `std`, so the three libc calls it wraps are made here
//! directly.
//!
//! # `dlopen` in a `no_std` process is not a `std` dependency
//!
//! The shared object on the other side of this call was compiled against the host's real
//! `std` and carries its own copy of it. That is fine and it is not the ban being broken: the
//! ban is on this compiler's own link line, which a link-line check reads directly. The
//! loaded object contributes nothing to it. It is opened `RTLD_LOCAL` so its symbols - its
//! allocator shim, its panic runtime, its lang items - stay in its own namespace and cannot be
//! bound to by anything already in this process.
//!
//! The two runtimes never share an allocation, either. That is what `proc_macro::bridge` is
//! for: every buffer that crosses carries its own `reserve` and `drop` function pointers, so
//! memory allocated on one side is only ever freed by the side that allocated it.
//!
//! # Why the bindings are declared here
//!
//! `ekostd` is the operating system for this tree and this belongs in it. It is published to
//! crates.io and consumed from the registry, so it cannot be edited from here; `libc` already
//! supplies the declarations and this crate already depends on it. `libc::dladdr` is used the
//! same way in `crate::rustc_session::filesearch`.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ffi::CStr;

use eko::path::Path;
use crate::rustc_fs_util::{path_to_c_string, try_canonicalize};

/// Which of the two calls failed, so the caller can say which in the diagnostic.
pub(crate) enum DylibError {
    /// The file could not be opened at all: it is not there, it is not a Mach-O/ELF shared
    /// object, or one of *its* dependencies could not be resolved.
    Open(String),
    /// It opened and the symbol is not in it. For a proc-macro dylib this means the file is a
    /// shared object built by a different compiler, or from a crate that is not a proc-macro
    /// crate at all.
    Sym(String),
}

impl DylibError {
    pub(crate) fn into_message(self) -> String {
        match self {
            DylibError::Open(err) => err,
            DylibError::Sym(err) => err,
        }
    }
}

/// Open `path` and read the value of the symbol named `sym_name` out of it, as a `T`.
///
/// # The indirection is easy to get wrong, so it is spelled out
///
/// `dlsym` returns the *address of the item*. The item here is a `static`, so that address is a
/// pointer to a `T` and the value wanted is what is at it - hence the `read` rather than a
/// transmute of the address itself. `libloading::Symbol<T>` gets the same answer by the opposite
/// route, transmuting the address to `T` and leaving the caller to write one more `*`; the two
/// spellings agree because upstream asks for a `*const &[ProcMacro]` where this asks for a
/// `&[ProcMacro]`.
///
/// # Safety
///
/// `T` must be the type of the symbol in the object at `path`. Nothing here can check that, and
/// there is no version tag on a `.dylib` to check it against: the guarantee comes from
/// `proc_macro::bridge` being a deliberately stable ABI, and from the `Client` type on this side
/// being byte-identical to the one the object was compiled with.
///
/// The returned value borrows from an object that is never unloaded, which is why `'static` is
/// available to the caller.
pub(crate) unsafe fn load_symbol_from_dylib<T: Copy>(
    path: &Path,
    sym_name: &str,
) -> Result<T, DylibError> {
    // `dlopen` treats a name with no `/` in it as a library to look for on the standard search
    // path, so a bare `libserde_derive-….dylib` would find some other file or nothing at all.
    // Resolving first also makes the error message name the file that was actually tried.
    let path = try_canonicalize(path).map_err(|err| DylibError::Open(err.to_string()))?;
    let c_path = path_to_c_string(&path);

    // `RTLD_LAZY | RTLD_LOCAL` is what `libloading::Library::new` uses, and both halves matter
    // here. Lazy because a proc-macro dylib carries a whole `std` and binding all of it eagerly
    // buys nothing. Local because this process has a runtime of its own: a `RTLD_GLOBAL` load
    // would put the object's `std` symbols where anything loaded afterwards could bind to them.
    //
    // SAFETY: `c_path` is NUL-terminated by `path_to_c_string` and outlives the call.
    let handle = unsafe { libc::dlopen(c_path.as_ptr().cast(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(DylibError::Open(last_dl_error().unwrap_or_else(|| "dlopen failed".to_string())));
    }

    // The handle is deliberately never `dlclose`d. A proc macro may leave statics, thread-locals
    // and interned data behind that outlive the expansion, and the `&'static [Client]` read
    // below points straight into the object's data segment. Upstream leaks it for the same
    // reason ("Intentionally leak the dynamic library"). In a one-shot compiler the leak is
    // collected by process exit; the process outlives a single compilation, so it keeps one handle per proc-macro crate
    // it has ever loaded, for the life of the session. That is bounded by the number of distinct
    // proc-macro crates in the inputs, not by the number of compiles.

    let mut c_sym = Vec::with_capacity(sym_name.len() + 1);
    c_sym.extend_from_slice(sym_name.as_bytes());
    c_sym.push(0);

    // `dlsym` reports "not found" by returning null, which is also a legal address for a symbol
    // to have, so the documented way to tell them apart is to clear the error first and read it
    // after. Clearing matters for a second reason here: a failed `dlopen` earlier in the process
    // leaves its message in place, and reporting that one against this symbol would be a lie.
    //
    // SAFETY: `handle` is a live handle from `dlopen` and `c_sym` is NUL-terminated.
    unsafe {
        libc::dlerror();
    }
    let addr = unsafe { libc::dlsym(handle, c_sym.as_ptr().cast()) };
    if addr.is_null() {
        return Err(DylibError::Sym(
            last_dl_error()
                .unwrap_or_else(|| format!("`{sym_name}` is not in this shared object")),
        ));
    }

    // SAFETY: the caller promises `T` is the type of that symbol. `addr` is the address of the
    // item, and the item is a `static T`.
    Ok(unsafe { addr.cast::<T>().read() })
}

/// The pending `dlerror` message, if there is one.
fn last_dl_error() -> Option<String> {
    // SAFETY: `dlerror` returns either null or a pointer to a NUL-terminated string owned by the
    // loader, valid until the next `dl*` call on this thread. It is copied out before returning.
    let err = unsafe { libc::dlerror() };
    if err.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(err) }.to_bytes();
    Some(String::from_utf8_lossy(bytes).into_owned())
}
