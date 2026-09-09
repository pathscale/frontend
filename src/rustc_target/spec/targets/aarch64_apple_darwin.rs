// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_target::spec::base::apple::{Arch, TargetEnv, base};
use crate::rustc_target::spec::{Os, SanitizerSet, Target, TargetMetadata, TargetOptions};

pub(crate) fn target() -> Target {
    let (opts, llvm_target, arch) = base(Os::MacOs, Arch::Arm64, TargetEnv::Normal);
    Target {
        llvm_target,
        metadata: TargetMetadata {
            description: Some("ARM64 Apple macOS (11.0+, Big Sur+)".into()),
            tier: Some(1),
            host_tools: Some(true),
            std: Some(true),
        },
        pointer_width: 64,
        data_layout: "e-m:o-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-n32:64-S128-Fn32"
            .into(),
        arch,
        options: TargetOptions {
            mcount: "\u{1}mcount".into(),
            cpu: "apple-m1".into(),
            max_atomic_width: Some(128),
            // FIXME: The leak sanitizer currently fails the tests, see #88132.
            supported_sanitizers: SanitizerSet::ADDRESS
                | SanitizerSet::CFI
                | SanitizerSet::THREAD
                | SanitizerSet::REALTIME,
            supports_xray: true,
            ..opts
        },
    }
}
