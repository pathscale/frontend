// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use eko::path::{Path, PathBuf};
use alloc::sync::Arc;

use rustc_macros::{Decodable, Encodable, StableHash};
use crate::rustc_target::spec::TargetTuple;

use crate::rustc_session::EarlyDiagCtxt;
use crate::rustc_session::filesearch::make_target_lib_path;

/// Directory containing object/library files, passed through the command-line `-L` flag.
#[derive(Clone, Debug)]
pub struct SearchPath {
    pub kind: PathKind,
    pub dir: Arc<Path>,
}

#[derive(PartialEq, Clone, Copy, Debug, Hash, Eq, Encodable, Decodable, StableHash)]
pub enum PathKind {
    Native,
    Crate,
    Dependency,
    Framework,
    All,
}

impl PathKind {
    pub fn matches(&self, kind: PathKind) -> bool {
        match (self, kind) {
            (PathKind::All, _) | (_, PathKind::All) => true,
            _ => *self == kind,
        }
    }
}

impl SearchPath {
    pub fn from_cli_opt(
        sysroot: &Path,
        triple: &TargetTuple,
        early_dcx: &EarlyDiagCtxt,
        path: &str,
        is_unstable_enabled: bool,
    ) -> Self {
        let (kind, path) = if let Some(stripped) = path.strip_prefix("native=") {
            (PathKind::Native, stripped)
        } else if let Some(stripped) = path.strip_prefix("crate=") {
            (PathKind::Crate, stripped)
        } else if let Some(stripped) = path.strip_prefix("dependency=") {
            (PathKind::Dependency, stripped)
        } else if let Some(stripped) = path.strip_prefix("framework=") {
            (PathKind::Framework, stripped)
        } else if let Some(stripped) = path.strip_prefix("all=") {
            (PathKind::All, stripped)
        } else {
            (PathKind::All, path)
        };
        let dir = match path.strip_prefix("@RUSTC_BUILTIN") {
            Some(stripped) => {
                if !is_unstable_enabled {
                    early_dcx.early_fatal(
                        "the `-Z unstable-options` flag must also be passed to \
                         enable the use of `@RUSTC_BUILTIN`",
                    );
                }

                make_target_lib_path(sysroot, triple.tuple()).join("builtin").join(stripped)
            }
            None => PathBuf::from(path),
        };
        if dir.as_os_str().is_empty() {
            early_dcx.early_fatal("empty search path given via `-L`");
        }

        Self::new(kind, dir)
    }

    pub fn from_sysroot_and_triple(sysroot: &Path, triple: &str) -> Self {
        Self::new(PathKind::All, make_target_lib_path(sysroot, triple))
    }

    fn new(kind: PathKind, dir: PathBuf) -> Self {
        SearchPath { kind, dir: dir.into() }
    }
}
