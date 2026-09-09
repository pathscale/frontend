use alloc::string::ToString;
#[cfg(feature = "nightly")]
use {
    rustc_macros::{Decodable_NoContext, Encodable_NoContext, StableHash},
    crate::rustc_span::{Symbol, sym},
};

/// Crate type, as specified by `#![crate_type]`
#[derive(Copy, Clone, Debug, Hash, PartialEq, Default, PartialOrd, Eq, Ord)]
#[cfg_attr(feature = "nightly", derive(StableHash, Encodable_NoContext, Decodable_NoContext))]
pub enum CrateType {
    /// `#![crate_type = "bin"]`
    Executable,
    /// `#![crate_type = "dylib"]`
    Dylib,
    /// `#![crate_type = "rlib"]` or `#![crate_type = "lib"]`
    #[default]
    Rlib,
    /// `#![crate_type = "staticlib"]`
    StaticLib,
    /// `#![crate_type = "cdylib"]`
    Cdylib,
    /// `#![crate_type = "proc-macro"]`
    ProcMacro,
    /// `#![crate_type = "sdylib"]`
    // Unstable; feature(export_stable)
    Sdylib,
}
#[cfg(feature = "nightly")]
impl CrateType {
    /// Pairs of each `#[crate_type] = "..."` value and the crate type it resolves to
    pub fn all() -> &'static [(Symbol, Self)] {
        debug_assert_eq!(CrateType::default(), CrateType::Rlib);
        &[
            (sym::lib, CrateType::Rlib),
            (sym::rlib, CrateType::Rlib),
            (sym::dylib, CrateType::Dylib),
            (sym::cdylib, CrateType::Cdylib),
            (sym::staticlib, CrateType::StaticLib),
            (sym::proc_dash_macro, CrateType::ProcMacro),
            (sym::bin, CrateType::Executable),
            (sym::sdylib, CrateType::Sdylib),
        ]
    }

    /// Same as [`CrateType::all`], but does not include unstable options.
    /// Used for diagnostics.
    pub fn all_stable() -> &'static [(Symbol, Self)] {
        debug_assert_eq!(CrateType::default(), CrateType::Rlib);
        &[
            (sym::lib, CrateType::Rlib),
            (sym::rlib, CrateType::Rlib),
            (sym::dylib, CrateType::Dylib),
            (sym::cdylib, CrateType::Cdylib),
            (sym::staticlib, CrateType::StaticLib),
            (sym::proc_dash_macro, CrateType::ProcMacro),
            (sym::bin, CrateType::Executable),
        ]
    }
}

impl CrateType {
    pub fn has_metadata(self) -> bool {
        match self {
            CrateType::Rlib | CrateType::Dylib | CrateType::ProcMacro => true,
            CrateType::Executable
            | CrateType::Cdylib
            | CrateType::StaticLib
            | CrateType::Sdylib => false,
        }
    }
}

#[cfg(feature = "nightly")]
impl TryFrom<Symbol> for CrateType {
    type Error = ();

    fn try_from(value: Symbol) -> Result<Self, Self::Error> {
        Ok(match value {
            sym::bin => CrateType::Executable,
            sym::dylib => CrateType::Dylib,
            sym::staticlib => CrateType::StaticLib,
            sym::cdylib => CrateType::Cdylib,
            sym::rlib => CrateType::Rlib,
            sym::lib => CrateType::default(),
            sym::proc_dash_macro => CrateType::ProcMacro,
            sym::sdylib => CrateType::Sdylib,
            _ => return Err(()),
        })
    }
}

impl core::fmt::Display for CrateType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            CrateType::Executable => "bin".fmt(f),
            CrateType::Dylib => "dylib".fmt(f),
            CrateType::Rlib => "rlib".fmt(f),
            CrateType::StaticLib => "staticlib".fmt(f),
            CrateType::Cdylib => "cdylib".fmt(f),
            CrateType::ProcMacro => "proc-macro".fmt(f),
            CrateType::Sdylib => "sdylib".fmt(f),
        }
    }
}

#[cfg(feature = "nightly")]
impl crate::rustc_error_messages::IntoDiagArg for CrateType {
    fn into_diag_arg(
        self,
        _: &mut crate::rustc_error_messages::LongTyPath,
    ) -> crate::rustc_error_messages::DiagArgValue {
        self.to_string().into_diag_arg(&mut None)
    }
}
