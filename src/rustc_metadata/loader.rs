//! Metadata input for dependencies.
//!
//! Loading an rlib or dylib is part of the frontend's crate graph. It used to live beside
//! object writing, which made backend output machinery a requirement even for a frontend-only
//! crate load.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use eko::file::File;
use eko::path::Path;

use object::{Object, ObjectSection};
use crate::rustc_data_structures::memmap::Mmap;
use crate::rustc_data_structures::owned_slice::{OwnedSlice, try_slice_owned};
use crate::rustc_target::spec::Target;
use tracing::debug;

use crate::rustc_metadata::creader::MetadataLoader;
use crate::rustc_metadata::fs::METADATA_FILENAME;

/// Reads rustc metadata from the dependency formats this fork supports.
#[derive(Debug)]
pub struct DefaultMetadataLoader;

fn load_metadata_with(
    path: &Path,
    f: impl for<'a> FnOnce(&'a [u8]) -> Result<&'a [u8], String>,
) -> Result<OwnedSlice, String> {
    let file =
        File::open(path).map_err(|e| format!("failed to open file '{}': {e}", path.display()))?;
    unsafe { Mmap::map(file) }
        .map_err(|e| format!("failed to mmap file '{}': {e}", path.display()))
        .and_then(|mmap| try_slice_owned(mmap, |mmap| f(mmap)))
}

impl MetadataLoader for DefaultMetadataLoader {
    fn get_rlib_metadata(&self, _target: &Target, path: &Path) -> Result<OwnedSlice, String> {
        debug!("getting rlib metadata for {}", path.display());
        load_metadata_with(path, |data| {
            let archive = object::read::archive::ArchiveFile::parse(data)
                .map_err(|e| format!("failed to parse rlib '{}': {e}", path.display()))?;
            for entry in archive.members() {
                let entry =
                    entry.map_err(|e| format!("failed to parse rlib '{}': {e}", path.display()))?;
                if entry.name() == METADATA_FILENAME.as_bytes() {
                    let data = entry
                        .data(data)
                        .map_err(|e| format!("failed to parse rlib '{}': {e}", path.display()))?;
                    return section(path, data, ".rmeta");
                }
            }
            Err(format!("metadata not found in rlib '{}'", path.display()))
        })
    }

    fn get_dylib_metadata(&self, _target: &Target, path: &Path) -> Result<OwnedSlice, String> {
        debug!("getting dylib metadata for {}", path.display());
        load_metadata_with(path, |data| section(path, data, ".rustc"))
    }
}

fn section<'a>(path: &Path, bytes: &'a [u8], name: &str) -> Result<&'a [u8], String> {
    let Ok(file) = object::File::parse(bytes) else {
        return Ok(bytes);
    };
    file.section_by_name(name)
        .ok_or_else(|| format!("no `{name}` section in '{}'", path.display()))?
        .data()
        .map_err(|e| format!("failed to read {name} section in '{}': {e}", path.display()))
}
