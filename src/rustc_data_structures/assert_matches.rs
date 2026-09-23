//! `assert_matches!` and `debug_assert_matches!`, defined here because core's are the unstable
//! `assert_matches` feature. `#[macro_export]` puts them at the crate root, so a file names them
//! with `use crate::assert_matches;` where upstream wrote `use std::assert_matches;`, and the
//! call sites are unchanged. The panic text follows core's, so a failure reads the same.

#[macro_export]
macro_rules! assert_matches {
    ($left:expr, $(|)? $($pattern:pat_param)|+ $(if $guard:expr)? $(,)?) => {
        match $left {
            $($pattern)|+ $(if $guard)? => {}
            ref left_val => {
                ::core::panic!(
                    "assertion `left matches right` failed\n  left: {:?}\n right: {}",
                    left_val,
                    ::core::stringify!($($pattern)|+ $(if $guard)?),
                );
            }
        }
    };
    ($left:expr, $(|)? $($pattern:pat_param)|+ $(if $guard:expr)?, $($arg:tt)+) => {
        match $left {
            $($pattern)|+ $(if $guard)? => {}
            ref left_val => {
                ::core::panic!(
                    "assertion `left matches right` failed: {}\n  left: {:?}\n right: {}",
                    ::core::format_args!($($arg)+),
                    left_val,
                    ::core::stringify!($($pattern)|+ $(if $guard)?),
                );
            }
        }
    };
}

#[macro_export]
macro_rules! debug_assert_matches {
    ($($arg:tt)*) => {
        if ::core::cfg!(debug_assertions) {
            $crate::assert_matches!($($arg)*);
        }
    };
}
