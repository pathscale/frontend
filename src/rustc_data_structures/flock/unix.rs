//! An advisory lock on a file, for the incremental-compilation directory.
//!
//! The lock itself was always `fcntl(F_SETLK)` straight through `libc`; only opening the file
//! went through `eko::file::OpenOptions`. That is now `libc-wrapper` too, which removes the last
//! reason this file needed `std`.
//!
//! The Hurd `cfg` that used to be here is gone with it. It selected `c_int` rather than
//! `c_short` for two `flock` fields on `hurd`-on-x86; this compiler has one target and carrying
//! a branch for a platform it cannot be built for is a branch nobody can test.

use alloc::boxed::Box;

use core::mem;
use eko::file::{self, File};
use eko::path::Path;

/// An advisory lock, released when dropped.
#[derive(Debug)]
pub struct Lock {
    file: File,
}

impl Lock {
    /// Take the lock, optionally waiting for it.
    pub fn new(p: &Path, wait: bool, create: bool, exclusive: bool) -> file::Result<Lock> {
        // Read-write because `fcntl` locking requires the descriptor to be open for the access
        // the lock covers, and an exclusive lock covers writing.
        let file = if create { File::create_rw(p) } else { File::open_rw(p) }?;

        let lock_type = if exclusive { libc::F_WRLCK } else { libc::F_RDLCK };

        let mut flock: libc::flock = unsafe { mem::zeroed() };
        flock.l_type = lock_type as libc::c_short;
        flock.l_whence = libc::SEEK_SET as libc::c_short;
        // Zero length means "to end of file, however long it becomes", which is what a whole-file
        // lock has to mean for a file still being written.
        flock.l_start = 0;
        flock.l_len = 0;

        let cmd = if wait { libc::F_SETLKW } else { libc::F_SETLK };
        let ret = unsafe { libc::fcntl(file.raw(), cmd, &flock) };
        if ret == -1 {
            return Err(file::Error::from_errno(eko::Errno::current(), "fcntl"));
        }
        Ok(Lock { file })
    }

    /// Whether the failure was the filesystem not supporting locks at all.
    ///
    /// A network filesystem that cannot lock is a reason to carry on without one, which is why
    /// this is a question the caller asks rather than an error it propagates.
    pub fn error_unsupported(err: &file::Error) -> bool {
        matches!(err.errno().0, libc::ENOTSUP | libc::ENOSYS)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let mut flock: libc::flock = unsafe { mem::zeroed() };
        flock.l_type = libc::F_UNLCK as libc::c_short;
        flock.l_whence = libc::SEEK_SET as libc::c_short;
        flock.l_start = 0;
        flock.l_len = 0;

        // Nothing to do if this fails: closing the descriptor releases the lock anyway, and this
        // is a destructor with nowhere to report.
        unsafe {
            libc::fcntl(self.file.raw(), libc::F_SETLK, &flock);
        }
    }
}

// `Box` is used by the callers' error type; naming it here keeps the import honest under no_std.
const _: Option<Box<()>> = None;
