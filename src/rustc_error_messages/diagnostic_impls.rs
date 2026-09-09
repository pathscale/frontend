// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_error_messages::LongTyPath;
use alloc::borrow::Cow;
use core::fmt;
use core::num::ParseIntError;
use eko::path::{Path, PathBuf};

use crate::rustc_span::edition::Edition;

use crate::rustc_error_messages::{DiagArgValue, IntoDiagArg};

pub struct DiagArgFromDisplay<'a>(pub &'a dyn fmt::Display);

impl IntoDiagArg for DiagArgFromDisplay<'_> {
    fn into_diag_arg(self, path: &mut LongTyPath) -> DiagArgValue {
        self.0.to_string().into_diag_arg(path)
    }
}

impl<'a> From<&'a dyn fmt::Display> for DiagArgFromDisplay<'a> {
    fn from(t: &'a dyn fmt::Display) -> Self {
        DiagArgFromDisplay(t)
    }
}

impl<'a, T: fmt::Display> From<&'a T> for DiagArgFromDisplay<'a> {
    fn from(t: &'a T) -> Self {
        DiagArgFromDisplay(t)
    }
}

impl<'a, T: Clone + IntoDiagArg> IntoDiagArg for &'a T {
    fn into_diag_arg(self, path: &mut LongTyPath) -> DiagArgValue {
        self.clone().into_diag_arg(path)
    }
}

#[macro_export]
macro_rules! into_diag_arg_using_display {
    ($( $ty:ty ),+ $(,)?) => {
        $(
            impl $crate::rustc_error_messages::IntoDiagArg for $ty {
                fn into_diag_arg(self, path: &mut $crate::rustc_error_messages::LongTyPath) -> $crate::rustc_error_messages::DiagArgValue {
                    $crate::rustc_error_messages::ToString::to_string(&self).into_diag_arg(path)
                }
            }
        )+
    }
}

macro_rules! into_diag_arg_for_number {
    ($( $ty:ty ),+ $(,)?) => {
        $(
            impl $crate::rustc_error_messages::IntoDiagArg for $ty {
                fn into_diag_arg(self, path: &mut $crate::rustc_error_messages::LongTyPath) -> $crate::rustc_error_messages::DiagArgValue {
                    // Convert to a string if it won't fit into `Number`.
                    #[allow(irrefutable_let_patterns)]
                    if let Ok(n) = TryInto::<i32>::try_into(self) {
                        $crate::rustc_error_messages::DiagArgValue::Number(n)
                    } else {
                        $crate::rustc_error_messages::ToString::to_string(&self).into_diag_arg(path)
                    }
                }
            }
        )+
    }
}

into_diag_arg_using_display!(
    eko::file::Error,
    core::fmt::Error,
    Box<dyn core::error::Error>,
    core::num::NonZero<u32>,
    Edition,
    crate::rustc_span::Ident,
    crate::rustc_span::MacroRulesNormalizedIdent,
    ParseIntError,
);
// The `IntoDiagArg for std::process::ExitStatus` entry was here. `eko::command::Output`
// reports `code: i32` and `success()`, nothing in the tree holds an `ExitStatus` any more, and
// no diagnostic named this impl, so it went out with the last `std::` in this crate.

into_diag_arg_for_number!(i8, u8, i16, u16, i32, u32, i64, u64, i128, u128, isize, usize);

impl IntoDiagArg for bool {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        if self {
            DiagArgValue::Str(Cow::Borrowed("true"))
        } else {
            DiagArgValue::Str(Cow::Borrowed("false"))
        }
    }
}

impl IntoDiagArg for char {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        DiagArgValue::Str(Cow::Owned(format!("{self:?}")))
    }
}

impl IntoDiagArg for Vec<char> {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        DiagArgValue::StrListSepByAnd(
            self.into_iter().map(|c| Cow::Owned(format!("{c:?}"))).collect(),
        )
    }
}

impl IntoDiagArg for crate::rustc_span::Symbol {
    fn into_diag_arg(self, path: &mut LongTyPath) -> DiagArgValue {
        self.to_ident_string().into_diag_arg(path)
    }
}

impl<'a> IntoDiagArg for &'a str {
    fn into_diag_arg(self, path: &mut LongTyPath) -> DiagArgValue {
        self.to_string().into_diag_arg(path)
    }
}

impl IntoDiagArg for String {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        DiagArgValue::Str(Cow::Owned(self))
    }
}

impl<'a> IntoDiagArg for Cow<'a, str> {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        DiagArgValue::Str(Cow::Owned(self.into_owned()))
    }
}

impl<'a> IntoDiagArg for &'a Path {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        DiagArgValue::Str(Cow::Owned(self.display().to_string()))
    }
}

impl IntoDiagArg for PathBuf {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        DiagArgValue::Str(Cow::Owned(self.display().to_string()))
    }
}

impl IntoDiagArg for alloc::ffi::CString {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        DiagArgValue::Str(Cow::Owned(self.to_string_lossy().into_owned()))
    }
}

impl IntoDiagArg for crate::rustc_data_structures::small_c_str::SmallCStr {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        DiagArgValue::Str(Cow::Owned(self.to_string_lossy().into_owned()))
    }
}

// `impl IntoDiagArg for std::backtrace::Backtrace` was here. There is no backtrace without
// `std`, and every diagnostic that carried one has already dropped the field, so nothing names
// the impl any more.
