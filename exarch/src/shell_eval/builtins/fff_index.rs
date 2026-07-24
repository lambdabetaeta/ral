//! The `fff` fuzzy-file-name index: process-global infrastructure behind the
//! thin [`builtin_fff`](super::builtin_fff) body.
//!
//! One [`Index`] per canonical base path is built on first use and leaked into
//! `&'static`, so its scan thread and filesystem watcher outlive any lock guard
//! handed back to a caller.  The registry hands out those borrows; each search
//! runs a fuzzy pass against the live picker.

use fff_search::file_picker::FilePicker;
use fff_search::{
    FFFMode, FilePickerOptions, FrecencyTracker, FuzzySearchOptions, PaginationArgs, QueryParser,
    SharedFilePicker, SharedFrecency,
};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How long to wait for the initial filesystem scan before serving
/// (possibly partial) results.  Big trees on slow disks can exceed
/// this; the index keeps populating in the background regardless.
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

/// One indexed tree, kept alive for the process lifetime.  The
/// [`SharedFilePicker`] owns the scan thread and the filesystem watcher;
/// dropping it would tear them down, but we never drop — the registry
/// hands out `&'static` borrows.
pub(super) struct Index {
    picker: SharedFilePicker,
}

/// Process-global registry: one `Index` per canonical base path.
/// Entries are leaked into `&'static` so the picker outlives any
/// lock guard returned to a caller.
fn registry() -> &'static Mutex<HashMap<PathBuf, &'static Index>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, &'static Index>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn resolve_base(base: &Path) -> ral_core::path::ResolvedPath {
    ral_core::path::Resolver::shell_less().resolve(&base.to_string_lossy())
}

/// Get-or-create the index for `base`.  Blocks the caller while the
/// initial scan runs the first time `base` is seen; cheap on every
/// subsequent call.
pub(super) fn index_for(base: &Path) -> Result<&'static Index, String> {
    let canonical = resolve_base(base)
        .canonicalise_strict()
        .map_err(|e| format!("could not canonicalise {}: {e}", base.display()))?;
    let mut guard = registry().lock().expect("fff registry mutex poisoned");
    if let Some(idx) = guard.get(&canonical) {
        return Ok(idx);
    }
    let idx: &'static Index = Box::leak(Box::new(build_index(&canonical)?));
    guard.insert(canonical, idx);
    drop(guard);
    Ok(idx)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:fff-db-dir] creates the fff index's temp db dir; cache infra, not turn-time data I/O"
)]
fn build_index(base: &Path) -> Result<Index, String> {
    let db_root = std::env::temp_dir().join(format!(
        "exarch-fff-{}-{:016x}",
        std::process::id(),
        path_hash(base),
    ));
    std::fs::create_dir_all(&db_root).map_err(|e| format!("fff db dir: {e}"))?;

    let frecency = SharedFrecency::default();
    frecency
        .init(FrecencyTracker::open(db_root.join("frecency")).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;

    let picker = SharedFilePicker::default();
    FilePicker::new_with_shared_state(
        picker.clone(),
        frecency,
        FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            mode: FFFMode::Ai,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    picker.wait_for_scan(SCAN_TIMEOUT);
    Ok(Index { picker })
}

fn path_hash(p: &Path) -> u64 {
    let mut h = DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// Run one search against `idx` and return matching paths.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the picker read guard must span both fuzzy_search and the result projection, which reads paths back through the picker"
)]
pub(super) fn search_paths(idx: &Index, query: &str, limit: usize) -> Result<Vec<String>, String> {
    let parser = QueryParser::default();
    let parsed = parser.parse(query);
    let picker_guard = idx
        .picker
        .read()
        .map_err(|e: fff_search::Error| e.to_string())?;
    let picker = picker_guard
        .as_ref()
        .ok_or("fff index handle is empty (scan failed)")?;
    let result = picker.fuzzy_search(
        &parsed,
        None,
        FuzzySearchOptions {
            pagination: PaginationArgs { offset: 0, limit },
            ..Default::default()
        },
    );
    Ok(result
        .items
        .iter()
        .map(|item| item.relative_path(picker))
        .collect())
}
