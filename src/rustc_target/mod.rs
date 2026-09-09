//! Some stuff used by rustc that doesn't have many dependencies
//!
//! Originally extracted from rustc::back, which was nominally the
//! compiler 'backend', though LLVM is rustc's backend, so rustc_target
//! is really just odds-and-ends relating to code gen and linking.
//! This crate mostly exists to make rustc smaller, so we might put
//! more 'stuff' here in the future. It does not have a dependency on
//! LLVM.

// tidy-alphabetical-start
// tidy-alphabetical-end

#![expect(internal_features)]
// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.

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
// No `std::` is left in this crate's `src/`; the only remaining reason to link std is the
// `#[test]` harness below, which is std's.
#[cfg(test)]
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use eko::path::{Path, PathBuf};

pub mod asm;
pub mod callconv;
pub mod json;
pub mod spec;
pub mod target_features;

#[cfg(test)]
mod tests;

/// The name of rustc's own place to organize libraries.
///
/// Used to be `rustc`, now the default is `rustlib`.
const RUST_LIB_DIR: &str = "rustlib";

/// Returns a `rustlib` path for this particular target, relative to the provided sysroot.
///
/// For example: `target_sysroot_path("/usr", "x86_64-unknown-linux-gnu")` =>
/// `"lib*/rustlib/x86_64-unknown-linux-gnu"`.
pub fn relative_target_rustlib_path(sysroot: &Path, target_triple: &str) -> PathBuf {
    let libdir = find_relative_libdir(sysroot);
    Path::new(libdir.as_ref()).join(RUST_LIB_DIR).join(target_triple)
}

/// The name of the directory rustc expects libraries to be located.
fn find_relative_libdir(sysroot: &Path) -> alloc::borrow::Cow<'static, str> {
    // FIXME: This is a quick hack to make the rustc binary able to locate
    // Rust libraries in Linux environments where libraries might be installed
    // to lib64/lib32. This would be more foolproof by basing the sysroot off
    // of the directory where `librustc_driver` is located, rather than
    // where the rustc binary is.
    // If --libdir is set during configuration to the value other than
    // "lib" (i.e., non-default), this value is used (see issue #16552).

    #[cfg(target_pointer_width = "64")]
    const PRIMARY_LIB_DIR: &str = "lib64";

    #[cfg(target_pointer_width = "32")]
    const PRIMARY_LIB_DIR: &str = "lib32";

    const SECONDARY_LIB_DIR: &str = "lib";

    match option_env!("CFG_LIBDIR_RELATIVE") {
        None | Some("lib") => {
            if sysroot.join(PRIMARY_LIB_DIR).join(RUST_LIB_DIR).exists() {
                PRIMARY_LIB_DIR.into()
            } else {
                SECONDARY_LIB_DIR.into()
            }
        }
        Some(libdir) => libdir.into(),
    }
}

macro_rules! target_spec_enum {
    (
        $( #[$attr:meta] )*
        pub enum $Name:ident {
            $(
                $( #[$variant_attr:meta] )*
                $Variant:ident = $string:literal $(,$alias:literal)* ,
            )*
        }
        parse_error_type = $parse_error_type:literal;
    ) => {
        $( #[$attr] )*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
        pub enum $Name {
            $(
                $( #[$variant_attr] )*
                $Variant,
            )*
        }

        impl FromStr for $Name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(match s {
                    $(
                        $string => Self::$Variant,
                        $($alias => Self::$Variant,)*
                    )*
                    _ => {
                        let all = [$( concat!("'", $string, "'") ),*].join(", ");
                        return Err(format!("invalid {}: '{s}'. allowed values: {all}", $parse_error_type));
                    }
                })
            }
        }

        impl $Name {
            pub const ALL: &'static [$Name] = &[ $( $Name::$Variant, )* ];
            pub fn desc(&self) -> &'static str {
                match self {
                    $( Self::$Variant => $string, )*
                }
            }
        }

        crate::rustc_target::target_spec_enum!(@common_impls $Name);
    };

    (
        $( #[$attr:meta] )*
        pub enum $Name:ident {
            $(
                $( #[$variant_attr:meta] )*
                $Variant:ident = $string:literal $(,$alias:literal)* ,
            )*
        }
        $( #[$other_variant_attr:meta] )*
        other_variant = $OtherVariant:ident;
    ) => {
        $( #[$attr] )*
        #[derive(Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
        pub enum $Name {
            $(
                $( #[$variant_attr:meta] )*
                $Variant,
            )*
            /// The vast majority of the time, the compiler deals with a fixed
            /// set of values, so it is convenient for them to be represented in
            /// an enum. However, it is possible to have arbitrary values in a
            /// target JSON file (which can be parsed when `--target` is
            /// specified). This might occur, for example, for an out-of-tree
            /// codegen backend that supports a value (e.g. architecture or OS)
            /// that rustc currently doesn't know about. This variant exists as
            /// an escape hatch for such cases.
            $( #[$other_variant_attr] )*
            $OtherVariant(crate::rustc_target::spec::StaticCow<str>),
        }

        impl FromStr for $Name {
            type Err = core::convert::Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(match s {
                    $(
                        $string => Self::$Variant,
                        $($alias => Self::$Variant,)*
                    )*
                    _ => Self::$OtherVariant(s.to_owned().into()),
                })
            }
        }

        impl $Name {
            pub fn desc(&self) -> &str {
                match self {
                    $( Self::$Variant => $string, )*
                    Self::$OtherVariant(name) => name.as_ref(),
                }
            }
        }

        crate::rustc_target::target_spec_enum!(@common_impls $Name);
    };

    (@common_impls $Name:ident) => {
        impl crate::rustc_target::json::ToJson for $Name {
            fn to_json(&self) -> crate::rustc_target::json::Json {
                self.desc().to_json()
            }
        }

        crate::rustc_target::json::serde_deserialize_from_str!($Name);


        impl core::fmt::Display for $Name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(self.desc())
            }
        }
    };
}
use target_spec_enum;
