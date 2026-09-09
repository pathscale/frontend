// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::JsonTimePassesEntry;

#[test]
fn with_rss() {
    let entry =
        JsonTimePassesEntry { pass: "typeck", time: 56.1, start_rss: Some(10), end_rss: Some(20) };

    assert_eq!(entry.to_string(), r#"{"pass":"typeck","time":56.1,"rss_start":10,"rss_end":20}"#)
}

#[test]
fn no_rss() {
    let entry = JsonTimePassesEntry { pass: "typeck", time: 56.1, start_rss: None, end_rss: None };

    assert_eq!(
        entry.to_string(),
        r#"{"pass":"typeck","time":56.1,"rss_start":null,"rss_end":null}"#
    )
}
