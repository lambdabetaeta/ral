//! Source positions.
//!
//! A [`Span`] is a half-open byte range `[start, end)` tagged with a
//! [`FileId`] — the opaque per-file handle carried on every span.
//! "No narrower position available" is `Option<Span>` = `None`, used
//! uniformly across AST, IR, and typechecker; there is no sentinel span.
//! Line/column recovery is deferred to render time: `diagnostic.rs` receives
//! the source text directly and asks `ariadne` to locate the line.

use serde::{Deserialize, Serialize};

/// A half-open byte range `[start, end)` within a single source file.
///
/// Spans are the primary source-location currency throughout the compiler.
/// They carry only byte offsets and a [`FileId`]; line/column recovery is
/// deferred to render time by handing the source text directly to `ariadne`.
///
/// `u32` offsets are ample: script inputs comfortably fit.
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
        debug_assert!(start <= end, "span start {start} > end {end}");
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
    pub fn join(self, other: Self) -> Self {
        debug_assert!(self.file == other.file, "join across files");
        Self {
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

/// Opaque handle into a [`SourceDb`](crate::diagnostic::SourceDb).
///
/// Each
/// registered source text gets a unique `FileId`; spans and runtime
/// locations carry these so diagnostics can recover the originating file at
/// render time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileId(pub u32);

impl FileId {
    /// A non-registered placeholder. Spans tagged `DUMMY` are tolerated but
    /// render without source context. Prefer a real `FileId` wherever we
    /// actually know the source.
    pub const DUMMY: Self = Self(u32::MAX);
}

impl Default for FileId {
    fn default() -> Self {
        Self::DUMMY
    }
}

/// A value paired with its optional source span — the uniform wrapper
/// used at every sub-position where a downstream pass may know a
/// narrower byte range than the enclosing node.
///
/// Shared by the lexer,
/// AST, IR, and typechecker: each per-position narrowing site
/// (`If.cond`, `Case.scrutinee`/`table`, per-arg and per-key positions,
/// `Force` operand, interpolation segments, …) is a [`Spanned<_>`]
/// wrapper rather than an ad-hoc `*_span` parallel field on the parent,
/// so one helper (`Inferencer::with_span` in the typechecker) can
/// drive narrowing uniformly downstream.
///
/// "No narrower position available" is `span: None`; the same encoding
/// used by [`Comp::span`](crate::ir::Comp) in the IR — there is no
/// sentinel `Span` and no bridging conversion.
///
/// `PartialEq` is structural: both `span` and `item` participate.
/// Parser test fixtures normalise every `Spanned`'s span to `None`
/// via `parser::tests::strip_one` (recurses through all
/// `Spanned`-bearing variants).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spanned<T> {
    pub span: Option<Span>,
    pub item: T,
}

impl<T> Spanned<T> {
    /// Construct with an explicit real span.
    pub fn new(span: Span, item: T) -> Self {
        Self {
            span: Some(span),
            item,
        }
    }

    /// Construct with no span — used in test fixtures and for
    /// elaborator-internal positions (hoisted applications, synthetic
    /// list/map elements) that have no source range to attribute.
    pub fn synthetic(item: T) -> Self {
        Self { span: None, item }
    }

    /// Construct with an already-optional span — used when threading
    /// a span from another `Spanned` or from elaborator state without
    /// repeated wrap/unwrap noise.
    pub fn with_span(span: Option<Span>, item: T) -> Self {
        Self { span, item }
    }
}

impl<T> Spanned<Box<T>> {
    /// Box `inner` and pair with the real span — the canonical shape of
    /// every AST sub-position that holds a `Spanned<Box<Ast>>` (`Force`,
    /// `Return`, `If.cond`, `Case.scrutinee`, …).  Saves the
    /// `Spanned::new(span, Box::new(inner))` boilerplate.
    pub fn boxed(span: Span, inner: T) -> Self {
        Self::new(span, Box::new(inner))
    }

    /// Synthetic-span counterpart of [`Self::boxed`] for test fixtures.
    pub fn synthetic_boxed(inner: T) -> Self {
        Self::synthetic(Box::new(inner))
    }
}

/// Normalise source text loaded from disk so Windows CRLF files parse the
/// same as LF files.  Source text is the shell language's input; lone
/// carriage returns are not meaningful there.
pub fn normalize_source_text(source: String) -> String {
    if source.contains('\r') {
        source.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        source
    }
}

// ── WithSpan ─────────────────────────────────────────────────────────

/// Save/restore a [`Span`] slot around a closure.  Both the
/// elaborator and the typechecker narrow their diagnostic position
/// this way — the trait avoids duplicating the same 6-line method.
pub(crate) trait WithSpan {
    fn span_slot(&mut self) -> &mut Option<Span>;

    fn with_span<T>(&mut self, sp: Option<Span>, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = *self.span_slot();
        if sp.is_some() {
            *self.span_slot() = sp;
        }
        let out = f(self);
        *self.span_slot() = saved;
        out
    }
}
