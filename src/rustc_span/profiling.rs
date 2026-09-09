// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::borrow::Borrow;

use crate::rustc_data_structures::profiling::EventArgRecorder;

use crate::rustc_span::RemapPathScopeComponents;
use crate::rustc_span::source_map::SourceMap;

/// Extension trait for self-profiling purposes: allows to record spans within a generic activity's
/// event arguments.
pub trait SpannedEventArgRecorder {
    /// Records the following event arguments within the current generic activity being profiled:
    /// - the provided `event_arg`
    /// - a string representation of the provided `span`
    ///
    /// Note: when self-profiling with costly event arguments, at least one argument
    /// needs to be recorded. A panic will be triggered if that doesn't happen.
    fn record_arg_with_span<A>(&mut self, source_map: &SourceMap, event_arg: A, span: crate::rustc_span::Span)
    where
        A: Borrow<str> + Into<String>;
}

impl SpannedEventArgRecorder for EventArgRecorder<'_> {
    fn record_arg_with_span<A>(&mut self, source_map: &SourceMap, event_arg: A, span: crate::rustc_span::Span)
    where
        A: Borrow<str> + Into<String>,
    {
        self.record_arg(event_arg);
        self.record_arg(source_map.span_to_string(span, RemapPathScopeComponents::DEBUGINFO));
    }
}
