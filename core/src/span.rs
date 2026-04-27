//! Source spans.
//!
//! A [`Span`] is a half-open byte range `[start, end)` within a single file
//! identified by a [`FileId`]. Line/column information is recovered at render
//! time from the [`SourceDb`](crate::source::SourceDb); we do not carry it on
//! nodes. Byte ranges are kept because they feed [`ariadne`] cleanly and
//! survive future editor integrations.
//!
//! `u32` offsets are ample: script inputs comfortably fit.

use crate::source::FileId;
use serde::{Deserialize, Serialize};

/// A half-open byte range `[start, end)` within a single source file.
///
/// Spans are the primary source-location currency throughout the compiler.
/// They carry only byte offsets and a [`FileId`]; line/column recovery is
/// deferred to render time via [`SourceDb`](crate::source::SourceDb).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    /// Byte offset of the first character in the span.
    pub start: u32,
    /// Byte offset one past the last character in the span.
    pub end: u32,
    /// Source file this span belongs to.
    pub file: FileId,
}

impl Span {
    /// Construct a span covering `[start, end)` in `file`.
    /// Panics (debug) if `start > end`.
    pub fn new(file: FileId, start: u32, end: u32) -> Self {
        debug_assert!(start <= end, "span start {} > end {}", start, end);
        Self { start, end, file }
    }

    /// A zero-width span at `pos`.
    pub fn point(file: FileId, pos: u32) -> Self {
        Self {
            start: pos,
            end: pos,
            file,
        }
    }

    /// Smallest span covering both `self` and `other`. Files must match.
    pub fn join(self, other: Span) -> Span {
        debug_assert!(self.file == other.file, "join across files");
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
            file: self.file,
        }
    }

    /// Number of bytes covered by the span.
    pub fn len(self) -> u32 {
        self.end - self.start
    }

    /// True when the span is zero-width (a cursor position).
    pub fn is_empty(self) -> bool {
        self.end == self.start
    }

    /// Convert to a `usize` range suitable for slicing source text.
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}
