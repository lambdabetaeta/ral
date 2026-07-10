//! Source positions and loaded source text.
//!
//! A [`Span`] is a half-open byte range `[start, end)` tagged with a
//! [`FileId`] — the opaque per-file handle carried on every span.
//! "No narrower position available" is `Option<Span>` = `None`, used
//! uniformly across AST, IR, and typechecker; there is no sentinel span.
//!
//! [`Source`] bundles a loaded text with a precomputed line-start index for
//! O(log lines) `byte → (line, col)` recovery, and [`SourceDb`] is the
//! per-turn registry that resolves a [`FileId`] back to its [`Source`] at
//! render time.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::text::floor_char_boundary;

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

    /// Convert to a `usize` range suitable for slicing source text.
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// Opaque handle into a [`SourceDb`].
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
/// so one helper (the crate-local [`WithSpan`] trait shared by the
/// elaborator and typechecker) can drive narrowing uniformly downstream.
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

// ── Loaded source text ───────────────────────────────────────────────

/// Source text bundled with a precomputed line-start index.
///
/// Binary search
/// over the index resolves a `(byte_offset → line, col)` lookup in
/// O(log lines), independent of file size; `eval_comp` recomputes `Location`
/// from a span on every node it visits, so the per-lookup cost is on a hot
/// path.
///
/// Built once when the source is loaded; `Arc<[u32]>` makes Location
/// clones (which happen every closure call) refcount-bumps rather
/// than copies.
#[derive(Clone, Debug)]
pub struct Source {
    /// Display name of the source — a script path, `<stdin>`, or a loaded
    /// module's virtual path.  Carried so a runtime error rendered from a
    /// [`SourceDb`] names the file the caret points into.
    name: Arc<str>,
    text: Arc<str>,
    /// Sorted byte offsets where each line starts.  `line_starts[0] == 0`;
    /// thereafter, `line_starts[i]` is the byte index immediately after the
    /// `i`-th newline.  Length is therefore one greater than the newline
    /// count in `text`.
    line_starts: Arc<[u32]>,
}

impl Source {
    /// Wrap `text` under display `name`, building the line-start index in
    /// one pass.
    pub fn new(name: Arc<str>, text: Arc<str>) -> Self {
        let mut starts: Vec<u32> =
            Vec::with_capacity(text.bytes().filter(|&b| b == b'\n').count() + 1);
        starts.push(0);
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "byte offset into a source that fits the u32 span system (< 4 GiB, compiler-standard)"
                )]
                starts.push((i + 1) as u32);
            }
        }
        Self {
            name,
            text,
            line_starts: starts.into(),
        }
    }

    /// Convenience: wrap `text` under `name` by allocating and indexing.
    pub fn from_text(name: &str, text: &str) -> Self {
        Self::new(Arc::from(name), Arc::from(text))
    }

    /// Borrow the source's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrow the underlying source text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Convert a byte offset into a 1-indexed (line, col) pair using the
    /// precomputed index.  O(log lines) for the line lookup, plus one
    /// `chars().count()` over the (typically short) current line for the
    /// column.
    pub fn byte_to_line_col(&self, byte_offset: usize) -> (usize, usize) {
        let safe = floor_char_boundary(&self.text, byte_offset);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "byte offset into a source that fits the u32 span system (< 4 GiB, compiler-standard)"
        )]
        let target = safe as u32;
        // Largest i such that line_starts[i] <= target.  partition_point
        // returns the first i where the predicate is false; subtract one.
        let line_idx = self
            .line_starts
            .partition_point(|&start| start <= target)
            .saturating_sub(1);
        let line_start = self.line_starts[line_idx] as usize;
        let line = line_idx + 1;
        let col = self.text[line_start..safe].chars().count() + 1;
        (line, col)
    }
}

/// Registry of every source text the current turn has loaded, keyed by
/// [`FileId`].
///
/// A [`SourceLoc`](crate::diagnostic::SourceLoc) carries the `FileId` of the
/// source whose `line`/`col` index it holds, and the runtime renderer
/// resolves that id here so a `source`d module's error draws its caret into
/// the module's own bytes rather than the top-level script's.
///
/// `Arc`-shared so the per-closure `Location` clone is a refcount bump.
/// Within a turn the top-level source and each module it loads each register
/// once; [`reset`](Self::reset) at the next turn boundary reclaims them.
#[derive(Clone, Debug, Default)]
pub struct SourceDb {
    sources: Arc<Vec<Source>>,
}

impl SourceDb {
    /// Register `source`, returning the [`FileId`] that resolves to it.
    pub fn register(&mut self, source: Source) -> FileId {
        let sources = Arc::make_mut(&mut self.sources);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "FileId is u32; a turn registers a handful of sources, far below 2^32"
        )]
        let id = FileId(sources.len() as u32);
        sources.push(source);
        id
    }

    /// Drop every registered source, returning the registry to empty so the
    /// next [`register`](Self::register) hands out [`FileId`] with index `0` again.
    /// Called at each top-level turn boundary so a long interactive session
    /// reclaims the prior turn's sources instead of growing without bound.
    pub fn reset(&mut self) {
        Arc::make_mut(&mut self.sources).clear();
    }

    /// Resolve `id` to its registered [`Source`], or `None` when the id is
    /// the placeholder [`FileId::DUMMY`] or names a source this registry
    /// does not hold.
    pub fn get(&self, id: FileId) -> Option<&Source> {
        self.sources.get(id.0 as usize)
    }

    /// Peek the [`FileId`] the next [`register`](Self::register) call will
    /// mint, without registering anything. Lets a caller stamp the id onto
    /// a program's spans *before* the source it names is itself registered
    /// — sound exactly when nothing else registers a source into this
    /// registry between the peek and that later registration.
    pub fn next_id(&self) -> FileId {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "FileId is u32; a turn registers a handful of sources, far below 2^32"
        )]
        FileId(self.sources.len() as u32)
    }
}

/// Convert a byte offset within `source` into a 1-indexed (line, col) pair.
///
/// Linear-scan version retained for the one-off caller that recovers a
/// position from source text without a cached [`Source`] to hand; hot paths
/// should build a [`Source`] once and call [`Source::byte_to_line_col`]
/// instead.
pub fn byte_to_line_col(source: &str, byte_offset: usize) -> (usize, usize) {
    let safe = floor_char_boundary(source, byte_offset);
    let prefix = &source[..safe];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let last_nl = prefix.rfind('\n');
    let line_start = last_nl.map_or(0, |i| i + 1);
    let col = source[line_start..safe].chars().count() + 1;
    (line, col)
}

/// Locate the byte offset in `source` corresponding to 1-indexed
/// (line, col).  `col` counts characters within the line, so the in-line
/// advance steps over `col - 1` characters to land on a char boundary.
pub(crate) fn line_col_to_byte(source: &str, line: usize, col: usize) -> usize {
    let mut byte_offset = 0usize;
    for (i, ln) in source.split_inclusive('\n').enumerate() {
        if i + 1 == line {
            let in_line = ln
                .char_indices()
                .nth(col.saturating_sub(1))
                .map_or(ln.len(), |(b, _)| b);
            return byte_offset + in_line;
        }
        byte_offset += ln.len();
    }
    byte_offset
}
