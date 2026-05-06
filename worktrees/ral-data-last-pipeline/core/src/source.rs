//! Source database and ariadne cache.
//!
//! Every piece of text that a diagnostic might reference — a script file, a
//! REPL submission, a synthetic prelude — is registered in a [`SourceDb`]
//! under a [`FileId`]. Spans (`core::span::Span`) carry byte offsets plus the
//! `FileId`; line/col is recovered at render time from the db.
//!
//! [`ariadne`] requires a `Cache<FileId>` to look up source text. We expose
//! one via [`SourceDb::cache`] — it lazily builds `ariadne::Source` values on
//! first access and keeps them inside the cache for the duration of a render.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Opaque handle into the [`SourceDb`]. Each registered source text gets a
/// unique `FileId`; spans carry these so diagnostics can recover the
/// originating file.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileId(pub u32);

impl FileId {
    /// A non-registered placeholder. Spans tagged `DUMMY` are tolerated but
    /// render without source context. Prefer a real `FileId` wherever we
    /// actually know the source.
    pub const DUMMY: FileId = FileId(u32::MAX);
}

/// A single registered source: its display name (e.g. file path or
/// `"<repl>"`) and the full text, ref-counted for cheap cloning into
/// ariadne caches.
pub struct SourceFile {
    /// Human-readable label shown in diagnostics (file path, `"<prelude>"`, etc.).
    pub name: String,
    /// Full source text. `Arc<str>` so the ariadne cache can hold a
    /// reference without copying.
    pub text: Arc<str>,
}

/// Append-only registry of source texts.
///
/// Sources are added with [`add`](SourceDb::add) and looked up by the
/// returned [`FileId`]. The db also produces an ariadne-compatible
/// [`Cache`](ariadne::Cache) for rendering diagnostics with source context.
#[derive(Default)]
pub struct SourceDb {
    files: Vec<SourceFile>,
}

impl SourceDb {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a source and return its id.
    pub fn add(&mut self, name: impl Into<String>, text: impl Into<Arc<str>>) -> FileId {
        let id = FileId(self.files.len() as u32);
        self.files.push(SourceFile {
            name: name.into(),
            text: text.into(),
        });
        id
    }

    /// Look up a source by id. Returns `None` for `FileId::DUMMY`.
    pub fn get(&self, id: FileId) -> Option<&SourceFile> {
        if id == FileId::DUMMY {
            return None;
        }
        self.files.get(id.0 as usize)
    }

    /// Display name for `id`, or `"<unknown>"` if not found.
    pub fn name(&self, id: FileId) -> &str {
        self.get(id).map(|f| f.name.as_str()).unwrap_or("<unknown>")
    }

    /// Full source text for `id`, if registered.
    pub fn text(&self, id: FileId) -> Option<&str> {
        self.get(id).map(|f| &*f.text)
    }

    /// Build an ariadne-compatible cache borrowing from this db.
    pub fn cache(&self) -> DbCache<'_> {
        DbCache {
            db: self,
            sources: HashMap::new(),
        }
    }
}

/// Ariadne cache adapter. Holds memoised `ariadne::Source` values so ariadne
/// can fetch sources by [`FileId`] during rendering.
pub struct DbCache<'a> {
    db: &'a SourceDb,
    sources: HashMap<FileId, ariadne::Source<Arc<str>>>,
}

impl<'a> ariadne::Cache<FileId> for DbCache<'a> {
    type Storage = Arc<str>;

    fn fetch(
        &mut self,
        id: &FileId,
    ) -> Result<&ariadne::Source<<Self as ariadne::Cache<FileId>>::Storage>, impl std::fmt::Debug>
    {
        match self.db.get(*id) {
            Some(f) => {
                let src = self
                    .sources
                    .entry(*id)
                    .or_insert_with(|| ariadne::Source::from(f.text.clone()));
                Ok(src)
            }
            None => Err(Box::new(format!("unknown FileId({})", id.0))),
        }
    }

    fn display<'b>(&self, id: &'b FileId) -> Option<impl std::fmt::Display + 'b> {
        Some(self.db.name(*id).to_string())
    }
}
