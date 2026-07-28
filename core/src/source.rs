//! Source positions and the loaded text they index.
//!
//! A [`Span`] is a half-open byte range tagged with a [`FileId`]; "no narrower
//! position known" is `Option<Span>` = `None` throughout the AST, IR, and
//! typechecker, and there is no sentinel span.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::text::floor_char_boundary;

/// A half-open byte range `[start, end)` within a single source file.
///
/// Carries only offsets and a [`FileId`]; line/column recovery waits until a
/// diagnostic renders and can be handed the source text itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub file: FileId,
}

impl Span {
    /// Construct a span covering `[start, end)` in `file`.
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

    /// Origin for host-synthesised bindings such as hook registrations.  The
    /// file must be [`FileId::DUMMY`]: the registry only grows, so a real id
    /// would claim that source's first bytes forever.
    pub fn synthetic() -> Self {
        Self {
            start: 0,
            end: 0,
            file: FileId::DUMMY,
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

    /// As a `usize` range, for slicing source text.
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// Opaque handle into a [`SourceDb`].  Spans and runtime locations carry one
/// so a diagnostic can recover the originating file at render time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileId(pub u32);

impl FileId {
    /// Never registered: spans tagged `DUMMY` render without source context.
    pub const DUMMY: Self = Self(u32::MAX);
}

impl Default for FileId {
    fn default() -> Self {
        Self::DUMMY
    }
}

/// A value paired with its optional source span.
///
/// Every narrowing site (`If.cond`, `Case.scrutinee`, per-argument positions,
/// interpolation segments, …) wraps in `Spanned` rather than growing a
/// parallel `*_span` field on its parent, so the one `WithSpan` helper narrows
/// them all alike.
///
/// `PartialEq` is structural — spans participate, which is why parser fixtures
/// strip them to `None` before comparing.
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

    /// Construct with no span, for elaborator-internal positions (hoisted
    /// applications, synthetic list/map elements) and test fixtures.
    pub fn synthetic(item: T) -> Self {
        Self { span: None, item }
    }

    /// Construct from an already-optional span.
    pub fn with_span(span: Option<Span>, item: T) -> Self {
        Self { span, item }
    }
}

impl<T> Spanned<Box<T>> {
    /// Box `inner` and pair it with `span`.
    pub fn boxed(span: Span, inner: T) -> Self {
        Self::new(span, Box::new(inner))
    }

    /// Span-less counterpart of [`Self::boxed`].
    pub fn synthetic_boxed(inner: T) -> Self {
        Self::synthetic(Box::new(inner))
    }
}

/// Fold CRLF and lone CR to LF, so a Windows-authored script parses like any
/// other; a carriage return means nothing in the shell language.  Every load
/// of source from disk or wire passes through here.
pub fn normalize_source_text(source: String) -> String {
    if source.contains('\r') {
        source.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        source
    }
}

/// Save and restore a [`Span`] slot around a closure — how both the
/// elaborator and the typechecker narrow their diagnostic position.
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

/// Source text with a line-start index built once at load, so `byte → (line,
/// col)` costs O(log lines) at any file size.  Cloning bumps refcounts rather
/// than copying the text.
#[derive(Clone, Debug)]
pub struct Source {
    /// A script path, `<stdin>`, or a module's virtual path — what a rendered
    /// error names as the file its caret points into.
    name: Arc<str>,
    text: Arc<str>,
    /// Ascending; `[0]` is always 0 and each later entry is the byte just past
    /// a newline, so the length is one more than the newline count.
    line_starts: Arc<[u32]>,
}

impl Source {
    /// Wrap `text` under display `name`, indexing the lines in one pass.
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

    /// Allocating counterpart of [`Self::new`].
    pub fn from_text(name: &str, text: &str) -> Self {
        Self::new(Arc::from(name), Arc::from(text))
    }

    /// The display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The source text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// A byte offset as a 1-indexed (line, col) pair: binary search for the
    /// line, then one `chars()` walk of that line for the column.
    pub fn byte_to_line_col(&self, byte_offset: usize) -> (usize, usize) {
        let safe = floor_char_boundary(&self.text, byte_offset);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "byte offset into a source that fits the u32 span system (< 4 GiB, compiler-standard)"
        )]
        let target = safe as u32;
        // partition_point yields the first line starting past `target`; the
        // line containing it is the one before.
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

/// Every source text the session has loaded, keyed by [`FileId`].  The
/// renderer resolves a span's id here, so a `source`d module's error draws its
/// caret into the module's own bytes rather than the top-level script's.
///
/// Append-only for the session's whole life: a nested run
/// ([`Shell::run_nested`](crate::Shell::run_nested)) shares the registry with
/// the run it nests in, so reclaiming a slot would re-mint a [`FileId`] the
/// outer run's live spans still carry.  Slots are `Option` because
/// `register_at` may land a source above ids this registry never minted.
#[derive(Clone, Debug, Default)]
pub struct SourceDb {
    sources: Arc<Vec<Option<Source>>>,
}

impl SourceDb {
    /// Register `source`, returning the [`FileId`] that resolves to it.
    pub fn register(&mut self, source: Source) -> FileId {
        let id = self.next_id();
        Arc::make_mut(&mut self.sources).push(Some(source));
        id
    }

    /// Place `source` under an id another registry minted — sound only for a
    /// process handed both across the wire, i.e. a re-exec'd pipeline-stage
    /// child resolving spans its parent compiled.
    pub(crate) fn register_at(&mut self, id: FileId, source: Source) {
        if id == FileId::DUMMY {
            return;
        }
        let sources = Arc::make_mut(&mut self.sources);
        let idx = id.0 as usize;
        if sources.len() <= idx {
            sources.resize(idx + 1, None);
        }
        sources[idx] = Some(source);
    }

    /// The [`Source`] `id` resolves to, or `None` for [`FileId::DUMMY`] and
    /// for ids this registry does not hold.
    pub fn get(&self, id: FileId) -> Option<&Source> {
        if id == FileId::DUMMY {
            return None;
        }
        self.sources.get(id.0 as usize)?.as_ref()
    }

    /// The [`FileId`] the next [`register`](Self::register) will mint, so a
    /// caller can stamp it onto spans before registering the source it names —
    /// sound exactly when nothing else registers in between.
    pub fn next_id(&self) -> FileId {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "FileId is u32; a run registers a handful of sources, far below 2^32"
        )]
        FileId(self.sources.len() as u32)
    }
}

/// 1-indexed (line, col) by linear scan, for a caller holding source text but
/// no [`Source`]; anything repeated should build one and use
/// [`Source::byte_to_line_col`].
pub fn byte_to_line_col(source: &str, byte_offset: usize) -> (usize, usize) {
    let safe = floor_char_boundary(source, byte_offset);
    let prefix = &source[..safe];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let last_nl = prefix.rfind('\n');
    let line_start = last_nl.map_or(0, |i| i + 1);
    let col = source[line_start..safe].chars().count() + 1;
    (line, col)
}
