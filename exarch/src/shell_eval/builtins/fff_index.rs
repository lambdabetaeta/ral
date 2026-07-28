//! Process-global `fff` fuzzy-file-name index, behind the `fff` builtin in
//! `exarch/src/shell_eval/builtins.rs`.
//!
//! One [`Index`] per canonical base, leaked into `&'static` so a caller's borrow
//! outlives the registry lock guard; nothing is dropped, so the scan threads and
//! watchers run for the life of the process.

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

/// Initial-scan budget; overrunning it serves partial results rather than failing.
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

/// One indexed tree; its [`SharedFilePicker`] owns the scan thread and the watcher.
pub(super) struct Index {
    picker: SharedFilePicker,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, &'static Index>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, &'static Index>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Sound only because `base` is already absolute: `shell_less` carries no `HOME`
/// to expand `~` against and no cwd to anchor to.
fn resolve_base(base: &Path) -> ral_core::path::ResolvedPath {
    ral_core::path::Resolver::shell_less().resolve(&base.to_string_lossy())
}

/// Get-or-create the index for `base`; the first call blocks on the initial scan.
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

/// The db path carries the pid, so concurrent exarchs never share a frecency store.
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

/// Fuzzy-search `idx`; paths come back relative to its base, which is the form
/// the caller's per-entry grant check expects.
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
