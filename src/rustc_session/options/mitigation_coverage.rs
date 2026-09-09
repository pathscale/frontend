// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::collections::{BTreeMap, BTreeSet};

use rustc_macros::{BlobDecodable, Encodable};
use crate::rustc_span::edition::Edition;
use crate::rustc_target::spec::StackProtector;

use crate::rustc_session::Session;
use crate::rustc_session::config::Options;
use crate::rustc_session::options::CFGuard;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encodable, BlobDecodable)]
pub enum DeniedPartialMitigationLevel {
    // Enabled(false) should be the bottom of the Ord hierarchy
    Enabled(bool),
    StackProtector(StackProtector),
}

impl DeniedPartialMitigationLevel {
    pub fn level_str(&self) -> &'static str {
        match self {
            DeniedPartialMitigationLevel::StackProtector(StackProtector::All) => "=all",
            DeniedPartialMitigationLevel::StackProtector(StackProtector::Basic) => "=basic",
            DeniedPartialMitigationLevel::StackProtector(StackProtector::Strong) => "=strong",
            // currently `=disabled` should not appear
            DeniedPartialMitigationLevel::Enabled(false) => "=disabled",
            DeniedPartialMitigationLevel::StackProtector(StackProtector::None)
            | DeniedPartialMitigationLevel::Enabled(true) => "",
        }
    }
}

impl core::fmt::Display for DeniedPartialMitigationLevel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DeniedPartialMitigationLevel::StackProtector(StackProtector::All) => {
                write!(f, "all")
            }
            DeniedPartialMitigationLevel::StackProtector(StackProtector::Basic) => {
                write!(f, "basic")
            }
            DeniedPartialMitigationLevel::StackProtector(StackProtector::Strong) => {
                write!(f, "strong")
            }
            DeniedPartialMitigationLevel::Enabled(true) => {
                write!(f, "enabled")
            }
            DeniedPartialMitigationLevel::StackProtector(StackProtector::None)
            | DeniedPartialMitigationLevel::Enabled(false) => {
                write!(f, "disabled")
            }
        }
    }
}

impl From<bool> for DeniedPartialMitigationLevel {
    fn from(value: bool) -> Self {
        DeniedPartialMitigationLevel::Enabled(value)
    }
}

impl From<StackProtector> for DeniedPartialMitigationLevel {
    fn from(value: StackProtector) -> Self {
        DeniedPartialMitigationLevel::StackProtector(value)
    }
}

#[derive(Copy, Clone)]
struct MitigationStatus {
    allowed: Option<bool>,
}

#[derive(Clone, Default)]
pub struct MitigationCoverageMap {
    map: BTreeMap<DeniedPartialMitigationKind, MitigationStatus>,
}

macro_rules! denied_partial_mitigations {
    ([$self:ident] enum $kind:ident {$(($name:ident, $text:expr, $since:ident, $code:expr)),*}) => {
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encodable, BlobDecodable)]
        pub enum DeniedPartialMitigationKind {
            $($name),*
        }

        impl core::fmt::Display for DeniedPartialMitigationKind {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                match self {
                    $(DeniedPartialMitigationKind::$name => write!(f, $text)),*
                }
            }
        }

        #[allow(unused)]
        impl DeniedPartialMitigationKind {
            pub fn allowed_by_default_at(&self, edition: Edition) -> bool {
                let denied_since = match self {
                    // Should change the denied-since edition of StackProtector to 2015
                    // (all editions) when `-C stack-protector` is stabilized.
                    $(DeniedPartialMitigationKind::$name => Edition::$since),*
                };
                edition < denied_since
            }
        }

        impl Options {
            pub fn all_denied_partial_mitigations(&self) -> impl Iterator<Item = DeniedPartialMitigationKind> {
                [$(DeniedPartialMitigationKind::$name),*].into_iter()
            }
        }

        impl Session {
            pub fn gather_enabled_denied_partial_mitigations(&$self) -> Vec<DeniedPartialMitigation> {
                let mut mitigations = [
                    $(
                    DeniedPartialMitigation {
                        kind: DeniedPartialMitigationKind::$name,
                        level: From::from($code),
                    }
                    ),*
                ];
                mitigations.sort();
                mitigations.into_iter().collect()
            }
        }
    }
}

denied_partial_mitigations! {
    [self]
    enum DeniedPartialMitigationKind {
        // The mitigation name should match the option name in crate::rustc_session::options,
        // to allow for resetting the mitigation
        (StackProtector, "stack-protector", EditionFuture, self.stack_protector()),
        (ControlFlowGuard, "control-flow-guard", EditionFuture, self.opts.cg.control_flow_guard == CFGuard::Checks)
    }
}

/// A mitigation that cannot be partially enabled (see
/// [RFC 3855](https://github.com/rust-lang/rfcs/pull/3855)), but are currently enabled for this
/// crate.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encodable, BlobDecodable)]
pub struct DeniedPartialMitigation {
    pub kind: DeniedPartialMitigationKind,
    pub level: DeniedPartialMitigationLevel,
}

impl Options {
    // Return the list of mitigations that are allowed to be partial
    pub fn allowed_partial_mitigations(
        &self,
        edition: Edition,
    ) -> impl Iterator<Item = DeniedPartialMitigationKind> {
        let mut result: BTreeSet<_> = self
            .all_denied_partial_mitigations()
            .filter(|mitigation| mitigation.allowed_by_default_at(edition))
            .collect();
        for (kind, MitigationStatus { allowed }) in &self.mitigation_coverage_map.map {
            match allowed {
                Some(true) => {
                    result.insert(*kind);
                }
                Some(false) => {
                    result.remove(kind);
                }
                None => {}
            }
        }
        result.into_iter()
    }
}
