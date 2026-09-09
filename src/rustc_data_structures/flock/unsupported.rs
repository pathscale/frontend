// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use eko::file as io;
use eko::path::Path;

#[derive(Debug)]
pub struct Lock(());

impl Lock {
    pub fn new(_p: &Path, _wait: bool, _create: bool, _exclusive: bool) -> io::Result<Lock> {
        let msg = "file locks not supported on this platform";
        Err(io::Error::new(io::ErrorKind::Other, msg))
    }

    pub fn error_unsupported(_err: &io::Error) -> bool {
        true
    }
}
