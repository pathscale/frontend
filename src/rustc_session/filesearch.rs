//! A module for searching for libraries

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use eko::path::{Path, PathBuf};
use crate::rustc_target::spec::Target;

use crate::rustc_session::search_paths::{PathKind, SearchPath};

pub struct FileSearch {
    cli_search_paths: Vec<SearchPath>,
    tlib_path: Option<SearchPath>,
    use_implicit_sysroot_deps: bool,
    files: Vec<FileSearchCandidate>,
}

impl FileSearch {
    pub fn cli_search_paths<'b>(&'b self, kind: PathKind) -> impl Iterator<Item = &'b SearchPath> {
        self.cli_search_paths.iter().filter(move |sp| sp.kind.matches(kind))
    }

    pub fn search_paths<'b>(&'b self, kind: PathKind) -> impl Iterator<Item = &'b SearchPath> {
        // If the crate is `PathKind::Crate` (a top level dependency)
        // and `-Z implicit-sysroot-deps=false`, then don't include the sysroot in the search paths.
        let exclude_sysroot = kind.matches(PathKind::Crate) && !self.use_implicit_sysroot_deps;
        let maybe_tlib = (!exclude_sysroot).then_some(self.tlib_path.as_ref()).flatten();

        self.cli_search_paths
            .iter()
            .filter(move |sp| sp.kind.matches(kind))
            .chain(maybe_tlib.into_iter())
    }

    /// Return files from the search dirs of this filesearch that match the given `prefix` and
    /// `suffix` and have the given `kind`.
    ///
    /// Note that this function only searches files that match lib/staticlib/dlllib prefixes, not
    /// all files from the search paths!
    /// Access `search_paths` directly if you want to scan all files within them.
    pub fn get_library_candidates<'b>(
        &'b self,
        prefix: &'b str,
        suffix: &'b str,
        kind: PathKind,
    ) -> impl Iterator<Item = (&'b str, PathBuf)> {
        let exclude_sysroot = kind.matches(PathKind::Crate) && !self.use_implicit_sysroot_deps;

        // The indices are clipped to have only a single iterator returned from this function, to
        // avoid allocating it.
        let start = self.files.partition_point(|v| *v.filename < *prefix).min(self.files.len());
        let end = self.files[start..].partition_point(|v| v.filename.starts_with(prefix));
        let prefixed_items = &self.files[start..][..end];

        prefixed_items
            .into_iter()
            .filter(move |c| {
                c.kind.matches(kind)
                    && !(exclude_sysroot && c.from_sysroot)
                    && c.filename.ends_with(suffix)
            })
            .map(|c| (&c.filename[prefix.len()..c.filename.len() - suffix.len()], c.path()))
    }

    pub fn new(
        cli_search_paths: &[SearchPath],
        tlib_path: Option<&SearchPath>,
        target: &Target,
        use_implicit_sysroot_deps: bool,
    ) -> Self {
        // We keep a list of all found paths that look like libraries in `FileSearch`, to optimize
        // lookup in `get_library_candidates`.
        // These prefixes should be kept in sync with `CrateLocator::find_library_crate`.
        let prefixes = ["lib", &target.staticlib_prefix, &target.dll_prefix];

        // Load all files from all search paths, filter them by supported prefixes, and sort them,
        // so that we can efficiently look them up in `get_file_candidates` via binary search.
        let mut files: Vec<FileSearchCandidate> = Vec::with_capacity(cli_search_paths.len());
        for (search_path, is_sysroot) in cli_search_paths
            .iter()
            .map(|path| (path, false))
            .chain(tlib_path.into_iter().map(|path| (path, true)))
        {
            let Ok(dir) = eko::file::read_dir(&search_path.dir) else {
                continue;
            };
            files.extend(dir.into_iter().filter_map(|entry| {
                // `read_dir` yields whole paths rather than entries, so the name is the last
                // component and there is no `Result` per entry to unwrap.
                let filename = entry.file_name()?.to_str()?;

                if !prefixes.iter().any(|prefix| filename.starts_with(prefix)) {
                    return None;
                }
                Some(FileSearchCandidate {
                    dir: Arc::clone(&search_path.dir),
                    filename: filename.into(),
                    kind: search_path.kind,
                    from_sysroot: is_sysroot,
                })
            }));
        }
        files.sort_unstable_by(|lhs, rhs| lhs.filename.cmp(&rhs.filename));

        FileSearch {
            cli_search_paths: cli_search_paths.to_owned(),
            tlib_path: tlib_path.cloned(),
            use_implicit_sysroot_deps,
            files,
        }
    }
}

/// This type stores `Box<str>` instead of `PathBuf` for the filename, because getting the
/// `file_name` of a `PathBuf` allocates, which is unnecessary. We have to go through the files
/// a lot of times, so storing file name and the directory separately saves time and memory.
///
/// The filename must be valid UTF-8. If it's not, the entry should be skipped, because all Rust
/// output files are valid UTF-8, and so a non-UTF-8 filename couldn't be one we're looking for.
#[derive(Debug)]
struct FileSearchCandidate {
    dir: Arc<Path>,
    filename: Box<str>,
    kind: PathKind,
    /// Was this file added through the target sysroot?
    from_sysroot: bool,
}

impl FileSearchCandidate {
    /// Constructs the full path to the file.
    fn path(&self) -> PathBuf {
        self.dir.join(&*self.filename)
    }
}

pub fn make_target_lib_path(sysroot: &Path, target_triple: &str) -> PathBuf {
    let rustlib_path = crate::rustc_target::relative_target_rustlib_path(sysroot, target_triple);
    sysroot.join(rustlib_path).join("lib")
}

/// Returns a path to the target's `bin` folder within its `rustlib` path in the sysroot. This is
/// where binaries are usually installed, e.g. the self-contained linkers, lld-wrappers, LLVM tools,
/// etc.
pub fn make_target_bin_path(sysroot: &Path, target_triple: &str) -> PathBuf {
    let rustlib_path = crate::rustc_target::relative_target_rustlib_path(sysroot, target_triple);
    sysroot.join(rustlib_path).join("bin")
}
