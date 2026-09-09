//! The filesystem operations the compiler makes that are not a plain read or write.
//!
//! Linking-or-copying an artefact, a scratch directory, and turning a path into something a C
//! API will take. All of it goes through `libc-wrapper` rather than `std`.
//!
//! # What went, and why it was safe to lose
//!
//! Two pieces of this crate existed only for Windows. `fix_windows_verbatim_for_gcc` stripped the
//! `\\?\` prefix that `fs::canonicalize` produced there, because msvcrt silently mistranslated it
//! and gcc appeared to reject the path; the comment above it pointed at rust-lang/rust#25505 from
//! 2015. `path_to_c_string` had a second body that went through `to_str().unwrap()` because
//! Windows paths are UTF-16 and could not be handed over as bytes.
//!
//! Neither has a body to keep here. This compiler targets one platform, a path is bytes, and
//! `canonicalize` is `realpath(3)` which does not produce verbatim prefixes. `fix_windows_..`
//! is kept as the identity it already was on Unix rather than deleted, because its caller in
//! `rustc_codegen_ssa` reads as a deliberate step and removing the call is that crate's decision.


// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// `#![no_std]` above is the ban and the compiler is the enforcer: without `extern crate std;`
// there is no `std` in the extern prelude, so any `std::` path fails to resolve and the build
// stops. Do not add that line back to make an error go away - the error is the point. Whatever
// needed `std` either has a `core`/`alloc` equivalent, belongs in `ekostd`, or is a
// dependency that has to be replaced.
//
// The prelude is the part a grep cannot see: `Vec`, `String`, `Box`, `format!`, `vec!`,
// `thread_local!` and `println!` name no path. Under `#![no_std]` they resolve through `alloc`
// and `eko` instead, which is why those imports appear at the top of every file here.
// ---------------------------------------------------------------------------------------------

use alloc::vec::Vec;

use eko::file;
use eko::path::{Path, PathBuf};

/// Historically stripped a Windows verbatim prefix. The identity function on this platform.
pub fn fix_windows_verbatim_for_gcc(p: &Path) -> PathBuf {
    p.to_path_buf()
}

/// Which of the two happened in [`link_or_copy`].
pub enum LinkOrCopy {
    /// A hard link was made.
    Link,
    /// The bytes were copied.
    Copy,
}

/// Hard-link `p` to `q`, falling back to a copy.
///
/// **The destination is not removed up front.** Creating a hard link fails when the destination
/// exists, and removing it defensively would cost every caller a syscall to serve the rare one -
/// incremental compilation calls this constantly and is built to avoid the failing case. So the
/// removal happens only after `link` reports `EEXIST`.
pub fn link_or_copy(p: impl AsRef<Path>, q: impl AsRef<Path>) -> file::Result<LinkOrCopy> {
    let p = p.as_ref();
    let q = q.as_ref();

    let err = match file::hard_link(p, q) {
        Ok(()) => return Ok(LinkOrCopy::Link),
        Err(err) => err,
    };

    if file::is_already_exists(&err) {
        file::remove_file(q);
        if file::hard_link(p, q).is_ok() {
            return Ok(LinkOrCopy::Link);
        }
    }

    // Linking failed for some other reason - a cross-device destination is the usual one.
    file::copy(p, q).map(|_| LinkOrCopy::Copy)
}

/// A path as NUL-terminated bytes, for a C API.
///
/// Returns the buffer rather than a `CString` because the only thing callers do with it is take
/// a pointer, and a `CString` would add an allocation and an interior-NUL check to a path that
/// came from the filesystem and cannot contain one.
pub fn path_to_c_string(p: &Path) -> Vec<u8> {
    p.as_c()
}

/// Resolve a path, falling back to making it absolute if it does not exist.
pub fn try_canonicalize(path: impl AsRef<Path>) -> file::Result<PathBuf> {
    let p = path.as_ref();
    file::canonicalize(p).or_else(|_| file::absolute(p))
}

/// Builds a scratch directory that removes itself when dropped.
#[derive(Default)]
pub struct TempDirBuilder<'a, 'b> {
    prefix: &'a str,
    suffix: &'b str,
}

impl<'a, 'b> TempDirBuilder<'a, 'b> {
    /// A builder with no prefix or suffix.
    pub fn new() -> Self {
        TempDirBuilder { prefix: "", suffix: "" }
    }

    /// Set the leading part of the name.
    pub fn prefix(&mut self, prefix: &'a str) -> &mut Self {
        self.prefix = prefix;
        self
    }

    /// Set the trailing part of the name.
    pub fn suffix(&mut self, suffix: &'b str) -> &mut Self {
        self.suffix = suffix;
        self
    }

    /// Create it under the system temporary directory.
    pub fn tempdir(&self) -> file::Result<file::TempDir> {
        file::TempDir::new(self.prefix, self.suffix)
    }

    /// Create it under `dir`.
    ///
    /// `rustc_metadata` passes the directory the output file is going to, so that moving the
    /// finished metadata into place stays a rename within one filesystem.
    pub fn tempdir_in(&self, dir: impl AsRef<Path>) -> file::Result<file::TempDir> {
        file::TempDir::new_in(dir.as_ref(), self.prefix, self.suffix)
    }
}
