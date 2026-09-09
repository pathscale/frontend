//! A scratch directory whose deletion can be called off.
//!
//! `-Zkeep-metadata-tmpdir` and the error paths both want the directory to survive, so the
//! decision to delete is a flag rather than a `Drop`. `ManuallyDrop` is how that is expressed:
//! the destructor runs the inner one only when `keep` is false.
//!
//! The `TempDir` is `libc-wrapper`'s rather than `tempfile`'s. `tempfile` was the only reason
//! this crate depended on it, and what it was providing - a directory under a chosen parent
//! that removes itself - is thirty lines of `mkdir` and `unlink`.

use core::mem::ManuallyDrop;

use eko::file::TempDir;
use eko::path::Path;

/// A `TempDir` that is only removed on drop when it was asked to be.
#[derive(Debug)]
pub struct MaybeTempDir {
    dir: ManuallyDrop<TempDir>,
    // Whether the TempDir should be deleted on drop.
    keep: bool,
}

impl Drop for MaybeTempDir {
    fn drop(&mut self) {
        // SAFETY: We are in the destructor, and no further access will occur.
        let dir = unsafe { ManuallyDrop::take(&mut self.dir) };
        if self.keep {
            // Forgetting it is what keeps it: `TempDir`'s own destructor is the thing that
            // removes the directory, so not running it leaves the directory on disk.
            core::mem::forget(dir);
        }
    }
}

impl AsRef<Path> for MaybeTempDir {
    fn as_ref(&self) -> &Path {
        self.dir.path()
    }
}

impl MaybeTempDir {
    /// Wrap a directory, saying whether to remove it on drop.
    pub fn new(dir: TempDir, keep_on_drop: bool) -> MaybeTempDir {
        MaybeTempDir { dir: ManuallyDrop::new(dir), keep: keep_on_drop }
    }
}
