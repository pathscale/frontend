// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

// `json.rs` no longer imports `Mutex` - the sink type it needed it for is gone - so the glob
// from `super` does not carry it here any more.
use eko::thread::Mutex;

use crate::rustc_span::BytePos;
use crate::rustc_span::source_map::FilePathMapping;
use serde::Deserialize;

use super::*;
use crate::rustc_errors::DiagCtxt;

#[derive(Deserialize, Debug, PartialEq, Eq)]
struct TestData {
    spans: Vec<SpanTestData>,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
struct SpanTestData {
    pub byte_start: u32,
    pub byte_end: u32,
    pub line_start: u32,
    pub column_start: u32,
    pub line_end: u32,
    pub column_end: u32,
}

// `JsonEmitter`'s sink is `core::fmt::Write` now, not `std::io::Write`, so the shared buffer
// is a `String` rather than a `Vec<u8>` and there is no `flush` to forward. `eko`'s
// `Mutex` does not poison, so `lock()` hands back the guard with nothing to `unwrap`.
struct Shared {
    data: Arc<Mutex<String>>,
}

impl Write for Shared {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.data.lock().push_str(s);
        Ok(())
    }
}

fn filename(sm: &SourceMap, path: &str) -> FileName {
    FileName::Real(sm.path_mapping().to_real_filename(sm.working_dir(), PathBuf::from(path)))
}

/// Test the span yields correct positions in JSON.
fn test_positions(code: &str, span: (u32, u32), expected_output: SpanTestData) {
    crate::rustc_span::create_default_session_globals_then(|| {
        let sm = Arc::new(SourceMap::new(FilePathMapping::empty()));
        sm.new_source_file(filename(&sm, "test.rs"), code.to_owned());

        let output = Arc::new(Mutex::new(String::new()));
        let je = JsonEmitter::new(
            Box::new(Shared { data: output.clone() }),
            Some(sm),
            true, // pretty
            HumanReadableErrorType { short: true, unicode: false },
            ColorConfig::Never,
        );

        let span = Span::with_root_ctxt(BytePos(span.0), BytePos(span.1));
        DiagCtxt::new(Box::new(je)).handle().span_err(span, "foo");

        let emitted = output.lock();
        let actual_output: TestData = serde_json::from_str(&emitted).unwrap();
        let spans = actual_output.spans;
        assert_eq!(spans.len(), 1);

        assert_eq!(expected_output, spans[0])
    })
}

#[test]
fn empty() {
    test_positions(
        " ",
        (0, 1),
        SpanTestData {
            byte_start: 0,
            byte_end: 1,
            line_start: 1,
            column_start: 1,
            line_end: 1,
            column_end: 2,
        },
    )
}

#[test]
fn bom() {
    test_positions(
        "\u{feff} ",
        (0, 1),
        SpanTestData {
            byte_start: 3,
            byte_end: 4,
            line_start: 1,
            column_start: 1,
            line_end: 1,
            column_end: 2,
        },
    )
}

#[test]
fn lf_newlines() {
    test_positions(
        "\nmod foo;\nmod bar;\n",
        (5, 12),
        SpanTestData {
            byte_start: 5,
            byte_end: 12,
            line_start: 2,
            column_start: 5,
            line_end: 3,
            column_end: 3,
        },
    )
}

#[test]
fn crlf_newlines() {
    test_positions(
        "\r\nmod foo;\r\nmod bar;\r\n",
        (5, 12),
        SpanTestData {
            byte_start: 6,
            byte_end: 14,
            line_start: 2,
            column_start: 5,
            line_end: 3,
            column_end: 3,
        },
    )
}

#[test]
fn crlf_newlines_with_bom() {
    test_positions(
        "\u{feff}\r\nmod foo;\r\nmod bar;\r\n",
        (5, 12),
        SpanTestData {
            byte_start: 9,
            byte_end: 17,
            line_start: 2,
            column_start: 5,
            line_end: 3,
            column_end: 3,
        },
    )
}

#[test]
fn span_before_crlf() {
    test_positions(
        "foo\r\nbar",
        (2, 3),
        SpanTestData {
            byte_start: 2,
            byte_end: 3,
            line_start: 1,
            column_start: 3,
            line_end: 1,
            column_end: 4,
        },
    )
}

#[test]
fn span_on_crlf() {
    test_positions(
        "foo\r\nbar",
        (3, 4),
        SpanTestData {
            byte_start: 3,
            byte_end: 5,
            line_start: 1,
            column_start: 4,
            line_end: 2,
            column_end: 1,
        },
    )
}

#[test]
fn span_after_crlf() {
    test_positions(
        "foo\r\nbar",
        (4, 5),
        SpanTestData {
            byte_start: 5,
            byte_end: 6,
            line_start: 2,
            column_start: 1,
            line_end: 2,
            column_end: 2,
        },
    )
}
