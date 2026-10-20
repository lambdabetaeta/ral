//! Source positions and the loaded text they index.
//!
//! A [`Span`] is a half-open byte range tagged with a [`FileId`]; "no narrower
//! position known" is `Option<Span>` = `None` throughout the AST, IR, and
//! typechecker.  A span into text no `SourceDb` holds carries [`FileId::DUMMY`].

use crate::path::NotUtf8;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A half-open byte range `[start, end)` within a single source file.
///
/// Carries only offsets and a [`FileId`]; line/column recovery waits until a
/// diagnostic renders and can be handed the source text itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub(crate) file: FileId,
}

impl Span {
    /// Construct a span covering `[start, end)` in `file`.
    pub fn new(file: FileId, start: u32, end: u32) -> Self {
        debug_assert!(start <= end, "span start {start} > end {end}");
        Self { start, end, file }
    }

    /// A zero-width span at `pos`.
    pub(crate) fn point(file: FileId, pos: u32) -> Self {
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
    pub(crate) fn with_span(span: Option<Span>, item: T) -> Self {
        Self { span, item }
    }

    /// Transform the item, keeping the span.
    pub(crate) fn map<U>(self, f: impl FnOnce(T) -> U) -> Spanned<U> {
        Spanned {
            span: self.span,
            item: f(self.item),
        }
    }
}

impl<T> Spanned<Box<T>> {
    /// Box `inner` and pair it with `span`.
    pub(crate) fn boxed(span: Span, inner: T) -> Self {
        Self::new(span, Box::new(inner))
    }

    /// Span-less counterpart of [`Self::boxed`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn synthetic_boxed(inner: T) -> Self {
        Self::synthetic(Box::new(inner))
    }
}

/// Drop one leading byte-order mark and fold CRLF and lone CR to LF, so a
/// Windows-authored script parses like any other.
///
/// Neither means anything in the shell language, and every load of source —
/// disk, wire, stdin, argv — passes through here.
pub fn normalize_source_text(mut source: String) -> String {
    if source.starts_with('\u{feff}') {
        source.remove(0);
    }
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

/// Where a source's text came from: its identity as a load, the key of the
/// module stack's cycle check.
///
/// A file is held by the native path it was read from, so the directory a
/// script-relative path resolves beside and the `$SCRIPT` it bakes are read
/// off the host's own name, never re-parsed from a rendering of it.  Text no
/// file holds is named only, for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceOrigin {
    /// A file on disk, by its native path.
    File(SourceFile),
    /// Text with no file behind it — the REPL, `-c`, a hook, an embedded
    /// profile — known only by the label a diagnostic shows, such as
    /// `<stdin>`.
    Virtual(Arc<str>),
}

/// The native path of a source file, UTF-8 by construction.
///
/// An origin travels serialized — in a run sent across the wire, on the
/// module stack, in the [`SourceDb`] — and serde holds a path only as text,
/// so a file whose path is not UTF-8 is refused here, before it is read,
/// rather than failing late at serialization.  Deserialization is no second
/// door: serde lifts a `PathBuf` only from UTF-8.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceFile(PathBuf);

impl SourceFile {
    /// The source file at `path`.
    ///
    /// # Errors
    /// [`NotUtf8`] when `path` is not UTF-8, naming it.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, NotUtf8> {
        let path = path.into();
        crate::path::utf8(&path)?;
        Ok(Self(path))
    }

    /// The native path.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl SourceOrigin {
    /// A virtual source labelled `label`.
    pub fn named(label: impl Into<Arc<str>>) -> Self {
        Self::Virtual(label.into())
    }

    /// The file this source was read from; `None` for a virtual source, which
    /// has no script identity.
    pub fn file(&self) -> Option<&Path> {
        match self {
            Self::File(file) => Some(file.path()),
            Self::Virtual(_) => None,
        }
    }
}

/// The name a diagnostic shows: a file's path [shown](crate::path::shown), a
/// virtual source's label as given.
impl std::fmt::Display for SourceOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(file) => f.write_str(&crate::path::shown(file.path())),
            Self::Virtual(label) => f.write_str(label),
        }
    }
}

/// Source text with a line-start index built once at load, so `byte → (line,
/// col)` costs O(log lines) at any file size.  Cloning bumps refcounts rather
/// than copying the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    origin: SourceOrigin,
    /// `origin` as shown, derived once at load: what a rendered error names as
    /// the file its caret points into, and what a [`CallSite`] carries.
    name: Arc<str>,
    text: Arc<str>,
    /// Ascending; `[0]` is always 0 and each later entry is the byte just past
    /// a newline, so the length is one more than the newline count.
    line_starts: Arc<[u32]>,
}

impl Source {
    /// Wrap `text` read from `origin`, indexing the lines in one pass.
    pub fn new(origin: SourceOrigin, text: Arc<str>) -> Self {
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
            name: origin.to_string().into(),
            origin,
            text,
            line_starts: starts.into(),
        }
    }

    /// Allocating counterpart of [`Self::new`].
    pub fn from_text(origin: SourceOrigin, text: &str) -> Self {
        Self::new(origin, Arc::from(text))
    }

    /// Where the text came from.
    pub fn origin(&self) -> &SourceOrigin {
        &self.origin
    }

    /// The display name.
    pub fn name(&self) -> &Arc<str> {
        &self.name
    }

    /// The source text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The source text, shared.
    pub fn text(&self) -> &Arc<str> {
        &self.text
    }

    /// A byte offset as a 1-indexed (line, col) pair: binary search for the
    /// line, then one `chars()` walk of that line for the column.
    pub(crate) fn line_col(&self, byte_offset: u32) -> (u32, u32) {
        let safe = self.text.floor_char_boundary(byte_offset as usize);
        // partition_point yields the first line starting past the offset; the
        // line containing it is the one before.
        let line_idx = self
            .line_starts
            .partition_point(|&start| start <= byte_offset)
            .saturating_sub(1);
        let line_start = self.line_starts[line_idx] as usize;
        let col = self.text[line_start..safe].chars().count() + 1;
        (saturate(line_idx + 1), saturate(col))
    }
}

/// A position's count, saturating: a source past 4 GiB already breaks spans.
fn saturate(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A [`Span`] resolved against the session's [`SourceDb`]: script name and
/// 1-indexed line/column, as it rides out on observations and capability checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub script: Arc<str>,
    pub line: u32,
    pub col: u32,
}

impl std::fmt::Display for CallSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}, line {}", self.script, self.line)
    }
}

/// Every source text the session has loaded, keyed by [`FileId`].
///
/// The renderer resolves a span's id here, so a `use`d module's error
/// draws its caret into the module's own bytes rather than the top-level
/// script's.
///
/// Append-only for the session's whole life: a module a run loads shares the
/// registry with that run, so reclaiming a slot would re-mint a [`FileId`]
/// the run's live spans still carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceDb {
    sources: Arc<Vec<Source>>,
}

impl SourceDb {
    /// Register `source`, returning the [`FileId`] that resolves to it.
    pub fn register(&mut self, source: Source) -> FileId {
        let id = self.next_id();
        Arc::make_mut(&mut self.sources).push(source);
        id
    }

    /// The [`Source`] `id` resolves to, or `None` for an id this registry
    /// does not hold — [`FileId::DUMMY`] among them, being `u32::MAX` and so
    /// past the end of any registry a session could fill.
    pub fn get(&self, id: FileId) -> Option<&Source> {
        self.sources.get(id.0 as usize)
    }

    /// `span` resolved to a script name and line/column; `None` when its
    /// source is not registered here.
    pub fn site(&self, span: Span) -> Option<CallSite> {
        let source = self.get(span.file)?;
        let (line, col) = source.line_col(span.start);
        Some(CallSite {
            script: source.name().clone(),
            line,
            col,
        })
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

impl Serialize for SourceDb {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.sources.iter().map(|s| (s.origin(), s.text())))
    }
}

impl<'de> Deserialize<'de> for SourceDb {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let pairs = Vec::<(SourceOrigin, Arc<str>)>::deserialize(deserializer)?;
        Ok(Self {
            sources: Arc::new(
                pairs
                    .into_iter()
                    .map(|(origin, text)| Source::new(origin, text))
                    .collect(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_source_text as normalize;

    #[test]
    fn one_leading_bom_is_stripped_before_line_endings_fold() {
        assert_eq!(normalize("\u{feff}echo hi".into()), "echo hi");
        assert_eq!(normalize("\u{feff}a\r\nb".into()), "a\nb");
        assert_eq!(normalize("\u{feff}\u{feff}x".into()), "\u{feff}x");
        assert_eq!(normalize("echo \u{feff}hi".into()), "echo \u{feff}hi");
    }

    /// A path no string can hold is refused as a source file where it
    /// enters, naming it, and never reaches a serializer.
    #[cfg(unix)]
    #[test]
    fn a_source_file_path_must_be_utf8() {
        use super::SourceFile;
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let bad = std::path::PathBuf::from(OsStr::from_bytes(b"/tmp/s\xffx.ral"));
        let err = SourceFile::new(bad).expect_err("not UTF-8");
        assert_eq!(err.to_string(), "/tmp/s\u{fffd}x.ral is not UTF-8");
        let good = SourceFile::new("/tmp/s.ral").expect("UTF-8");
        assert_eq!(good.path(), std::path::PathBuf::from("/tmp/s.ral"));
    }
}
