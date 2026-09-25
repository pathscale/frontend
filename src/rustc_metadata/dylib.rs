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
/// `T` must be the type of the symbol in the object at `path`. Nothing here can check that; the
/// compiler that wrote the file is readable ([`rustc_version_of_dylib`]) and a caller that did not
/// build the file itself checks it first ([`release_proc_macro_table`]). Past that the guarantee
/// comes from
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

/// The name to `dlsym` for a proc-macro dylib's table, found by reading its symbols rather than
/// by computing it.
///
/// The name ends in the crate's `StableCrateId`, which rustc hashed from the crate's name, the
/// `-C metadata` values cargo passed and the compiler's own version. None of those three is this
/// read's to know (the crate was read from source here, under an id of this read's choosing), so
/// the id is taken from the one place it is written down: the symbol itself. A proc-macro dylib
/// exports exactly one symbol of this shape; none, or two, is refused.
///
/// Mach-O prefixes every C symbol with `_` in the file and `dlsym` adds it back itself, so the
/// name returned starts at `__rustc_proc_macro_decls_` whatever came before it.
pub(crate) fn find_proc_macro_decls_symbol(path: &Path) -> Result<String, DylibError> {
    use object::{Object, ObjectSymbol};

    let open_err = |err: String| DylibError::Open(format!("{}: {err}", path.display()));
    let file = eko::file::File::open(path).map_err(|e| open_err(e.to_string()))?;
    // SAFETY: the mapping is read-only and dropped before this returns; a file changed under it
    // meanwhile gives wrong bytes, not unsoundness in what is copied out.
    let mmap = unsafe { crate::rustc_data_structures::memmap::Mmap::map(file) }
        .map_err(|e| open_err(e.to_string()))?;
    let bytes: &[u8] = &mmap;
    let object = object::File::parse(bytes).map_err(|e| open_err(e.to_string()))?;

    let mut found: Vec<String> = Vec::new();
    for symbol in object.symbols().chain(object.dynamic_symbols()) {
        if !symbol.is_definition() || !symbol.is_global() {
            continue;
        }
        let Ok(name) = symbol.name_bytes() else { continue };
        if let Some(name) = proc_macro_decls_name(name)
            && !found.iter().any(|seen| *seen == name)
        {
            found.push(name);
        }
    }
    match found.len() {
        1 => Ok(found.pop().unwrap()),
        0 => Err(DylibError::Sym(format!(
            "{} exports no `__rustc_proc_macro_decls_*__` symbol: it is not a proc-macro crate's shared object",
            path.display()
        ))),
        _ => Err(DylibError::Sym(format!(
            "{} exports {} proc-macro tables ({}), and a proc-macro crate writes one",
            path.display(),
            found.len(),
            found.join(", ")
        ))),
    }
}

/// `name` as `dlsym` wants it, when it is a proc-macro table's symbol: `__rustc_proc_macro_decls_`,
/// hexadecimal digits, `__`, after at most the one `_` Mach-O prefixes.
fn proc_macro_decls_name(name: &[u8]) -> Option<String> {
    const PREFIX: &[u8] = b"__rustc_proc_macro_decls_";
    let name = match name.strip_prefix(b"_") {
        Some(rest) if rest.starts_with(PREFIX) => rest,
        _ => name,
    };
    let id = name.strip_prefix(PREFIX)?.strip_suffix(b"__")?;
    if id.is_empty() || !id.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    core::str::from_utf8(name).ok().map(str::to_string)
}

/// The version string of the compiler that built the shared object at `path`, as its metadata
/// states it (`rustc 1.97.1 (8bab26f4f 2026-07-14)`).
///
/// A dylib carries its metadata in a `.rustc` section: the metadata header, the blob's length as
/// eight little-endian bytes, then the blob. The blob opens with the header again and the root's
/// position, eight bytes more, and then the version string. That opening is the part rustc keeps
/// readable across metadata formats, precisely so one compiler can say which other compiler
/// wrote a file it cannot read (`MetadataBlob::check_compatibility` reads it at the same offset),
/// so it is read here without decoding anything else.
pub(crate) fn rustc_version_of_dylib(path: &Path) -> Result<String, DylibError> {
    use object::{Object, ObjectSection};

    let open_err = |err: String| DylibError::Open(format!("{}: {err}", path.display()));
    let file = eko::file::File::open(path).map_err(|e| open_err(e.to_string()))?;
    // SAFETY: as in `find_proc_macro_decls_symbol`.
    let mmap = unsafe { crate::rustc_data_structures::memmap::Mmap::map(file) }
        .map_err(|e| open_err(e.to_string()))?;
    let bytes: &[u8] = &mmap;
    let object = object::File::parse(bytes).map_err(|e| open_err(e.to_string()))?;
    let section = object
        .section_by_name(".rustc")
        .ok_or_else(|| open_err("no `.rustc` section: not a shared object rustc wrote".to_string()))?;
    let data = section.data().map_err(|e| open_err(e.to_string()))?;
    version_in_rustc_section(data)
        .ok_or_else(|| open_err("its `.rustc` section states no compiler version".to_string()))
}

/// The version string in the bytes of a `.rustc` section; see [`rustc_version_of_dylib`].
fn version_in_rustc_section(data: &[u8]) -> Option<String> {
    // `rust\0\0\0` and a format byte, which differs between compilers and is not checked.
    const MAGIC: &[u8] = b"rust\0\0\0";
    const HEADER: usize = 8;
    if !data.starts_with(MAGIC) {
        return None;
    }
    let blob = data.get(HEADER + 8..)?;
    if !blob.starts_with(MAGIC) {
        return None;
    }
    let mut rest = blob.get(HEADER + 8..)?;
    // The string's length, LEB128, as `rustc_serialize` writes a `usize`.
    let mut len: usize = 0;
    let mut shift = 0u32;
    loop {
        let (&byte, tail) = rest.split_first()?;
        rest = tail;
        len |= usize::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= usize::BITS {
            return None;
        }
    }
    let text = core::str::from_utf8(rest.get(..len)?).ok()?;
    text.starts_with("rustc ").then(|| text.to_string())
}

/// The stable releases whose proc-macro dylibs this tree's bridge server can run, as
/// `(major, minor)`.
///
/// What was compared, for each: `library/proc_macro/src/bridge` of the release's `rust-src`
/// against `src/rustc_proc_macro/bridge`. The method list in `with_api!` (whose order numbers the
/// tags on the wire), `BridgeConfig`, `Buffer` and `Closure` (`#[repr(C)]`, passed by value
/// through `extern "C"`), the encodings of every argument and result type, `run_server`'s
/// `(ExpnGlobals, input)` and the client's `Result` reply are the same; the one difference is the
/// table's element, `ProcMacro` around a `Client` that still carries `handle_counters`, which
/// `bridge::client::CountedProcMacro` mirrors. A release is added here only after the same
/// comparison of its `rust-src`: a wire that differs by one method runs the wrong one.
pub(crate) const COUNTED_TABLE_RELEASES: &[(u32, u32)] = &[(1, 97)];

/// Whether `version` (`rustc 1.97.1 (...)`) is one of [`COUNTED_TABLE_RELEASES`].
pub(crate) fn runs_release(version: &str) -> bool {
    let Some(number) = version.strip_prefix("rustc ").and_then(|v| v.split(' ').next()) else {
        return false;
    };
    // A `-beta.N` or `-nightly` suffix is a different bridge from the release's.
    if number.contains('-') {
        return false;
    }
    let mut parts = number.split('.').map(str::parse::<u32>);
    let (Some(Ok(major)), Some(Ok(minor))) = (parts.next(), parts.next()) else {
        return false;
    };
    COUNTED_TABLE_RELEASES.contains(&(major, minor))
}

/// The proc-macro table of the dylib at `path`, which a stable release built: the compiler that
/// wrote it checked against [`COUNTED_TABLE_RELEASES`] first, then its one table found by
/// [`find_proc_macro_decls_symbol`] and read. Opening it runs the dylib's initializers.
pub(crate) fn release_proc_macro_table(
    path: &Path,
) -> Result<&'static [crate::rustc_proc_macro::bridge::client::CountedProcMacro], DylibError> {
    let version = rustc_version_of_dylib(path)?;
    if !runs_release(&version) {
        let runs: Vec<String> =
            COUNTED_TABLE_RELEASES.iter().map(|(major, minor)| format!("{major}.{minor}")).collect();
        return Err(DylibError::Open(format!(
            "{} was built by `{version}`, and this frontend's proc-macro bridge runs the dylibs of rustc {} only",
            path.display(),
            runs.join(", ")
        )));
    }
    let sym_name = find_proc_macro_decls_symbol(path)?;
    // SAFETY: the release that wrote this dylib is one whose table element is
    // `CountedProcMacro` (checked just above), and the symbol is that table.
    unsafe { load_symbol_from_dylib(path, &sym_name) }
}

/// [`release_proc_macro_table`] as `(kind, name)` pairs; see
/// `frontend_facts::proc_macro_dylib_macros`.
pub(crate) fn proc_macro_dylib_macros(path: &Path) -> Result<Vec<(&'static str, String)>, String> {
    use crate::rustc_proc_macro::bridge::client::CountedProcMacro;

    let table = release_proc_macro_table(path).map_err(DylibError::into_message)?;
    Ok(table
        .iter()
        .map(|entry| match *entry {
            CountedProcMacro::CustomDerive { trait_name, .. } => ("derive", trait_name.to_string()),
            CountedProcMacro::Attr { name, .. } => ("attr", name.to_string()),
            CountedProcMacro::Bang { name, .. } => ("bang", name.to_string()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decls_symbol_is_recognised_with_and_without_the_mach_o_underscore() {
        assert_eq!(
            proc_macro_decls_name(b"___rustc_proc_macro_decls_3a8cb451bd7d5780__").as_deref(),
            Some("__rustc_proc_macro_decls_3a8cb451bd7d5780__")
        );
        assert_eq!(
            proc_macro_decls_name(b"__rustc_proc_macro_decls_3a8cb451bd7d5780__").as_deref(),
            Some("__rustc_proc_macro_decls_3a8cb451bd7d5780__")
        );
        assert_eq!(proc_macro_decls_name(b"__rustc_proc_macro_decls___"), None);
        assert_eq!(proc_macro_decls_name(b"__rustc_proc_macro_decls_xyz__"), None);
        assert_eq!(proc_macro_decls_name(b"_rust_eh_personality"), None);
    }

    fn section(version: &str) -> Vec<u8> {
        let mut blob = b"rust\0\0\0\x09".to_vec();
        blob.extend_from_slice(&[0; 8]);
        blob.push(version.len() as u8);
        blob.extend_from_slice(version.as_bytes());
        blob.push(0xc1);
        let mut data = b"rust\0\0\0\x09".to_vec();
        data.extend_from_slice(&(blob.len() as u64).to_le_bytes());
        data.extend_from_slice(&blob);
        data
    }

    #[test]
    fn the_version_is_read_from_a_rustc_section() {
        let version = "rustc 1.97.1 (8bab26f4f 2026-07-14)";
        assert_eq!(version_in_rustc_section(&section(version)).as_deref(), Some(version));
        assert_eq!(version_in_rustc_section(b"rust\0\0\0"), None);
        assert_eq!(version_in_rustc_section(b"not metadata at all, not even close"), None);
    }

    #[test]
    fn only_a_compared_release_runs() {
        assert!(runs_release("rustc 1.97.1 (8bab26f4f 2026-07-14)"));
        assert!(runs_release("rustc 1.97.0 (00000000 2026-07-01)"));
        assert!(!runs_release("rustc 1.98.0 (88d9e12ae 2026-08-18)"));
        assert!(!runs_release("rustc 1.97.0-beta.3 (00000000 2026-06-01)"));
        assert!(!runs_release("rustc 1.100.0-nightly (fd7ed57df 2026-08-29)"));
        assert!(!runs_release("garbage"));
    }
}
