//! `fff` tool — fuzzy filename search via the `fff-search` crate.
//!
//! On the first call against a given directory the tool spawns
//! `FilePicker`'s background scan + filesystem watcher and blocks
//! until the scan settles (or [`SCAN_TIMEOUT`] elapses).  The picker
//! is then cached in a process-global registry keyed by the
//! canonicalised base path, so subsequent calls — including those
//! from forked sub-agents that share the same cwd — reuse the index
//! and live on the watcher's incremental updates.
//!
//! The frecency and query-history databases live under
//! `$TMPDIR/exarch-fff-<pid>-<hash>/` and are reaped when the process
//! exits; LMDB locking keeps two concurrent `exarch` processes from
//! colliding because each pid gets a disjoint directory.

use super::{Tool, invalid_input};
use crate::bus::{Emitter, Kind};
use crate::digest::{FFF_CAP, clip};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use crate::session::Session;
use fff_search::file_picker::FilePicker;
use fff_search::{
    FFFMode, FilePickerOptions, FrecencyTracker, FuzzySearchOptions, PaginationArgs, QueryParser,
    QueryTracker, SharedFilePicker, SharedFrecency, SharedQueryTracker,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub(super) struct FffTool;

/// How long to wait for the initial filesystem scan before serving
/// (possibly partial) results.  Big trees on slow disks can exceed
/// this; the index keeps populating in the background regardless.
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// One indexed tree, kept alive for the process lifetime.  The
/// [`SharedFilePicker`] owns the scan thread and the filesystem watcher;
/// dropping it would tear them down, but we never drop — the registry
/// hands out `&'static` borrows.
struct Index {
    picker: SharedFilePicker,
    queries: SharedQueryTracker,
}

#[cfg_attr(test, derive(Debug))]
struct FffArgs {
    query: String,
    limit: usize,
}

fn parse_args(input: &Value) -> Result<FffArgs, String> {
    let obj = input
        .as_object()
        .ok_or_else(|| "tool input is not a JSON object".to_string())?;
    let query = obj
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required string field `query`".to_string())?
        .to_string();
    let limit = match obj.get("limit") {
        None => DEFAULT_LIMIT,
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| "field `limit` must be a non-negative integer".to_string())?
                as usize;
            n.clamp(1, MAX_LIMIT)
        }
    };
    Ok(FffArgs { query, limit })
}

/// Process-global registry: one `Index` per canonical base path.
/// Entries are leaked into `&'static` so the picker outlives any
/// lock guard returned to a caller.
fn registry() -> &'static Mutex<HashMap<PathBuf, &'static Index>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, &'static Index>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Mint a [`ResolvedPath`] for an already-absolute `base` through a
/// shell-less resolver — the public door to canonicalisation.  `base`
/// is absolute, so the empty cwd never anchors it.
fn resolve_base(base: &Path) -> ral_core::path::ResolvedPath {
    ral_core::path::Resolver {
        home: String::new(),
        cwd: None,
        mode: ral_core::path::CanonMode::Lenient,
    }
    .resolve(&base.to_string_lossy())
}

/// Get-or-create the index for `base`.  Blocks the caller while the
/// initial scan runs the first time `base` is seen; cheap on every
/// subsequent call.
fn index_for(base: &Path) -> Result<&'static Index, String> {
    let canonical = resolve_base(base)
        .canonicalise_strict()
        .map_err(|e| format!("could not canonicalise {}: {e}", base.display()))?;
    let mut guard = registry().lock().expect("fff registry mutex poisoned");
    if let Some(idx) = guard.get(&canonical) {
        return Ok(idx);
    }
    let idx: &'static Index = Box::leak(Box::new(build_index(&canonical)?));
    guard.insert(canonical, idx);
    Ok(idx)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:fff-db-dir] creates the fff index's temp db dir; tool cache infra, not turn-time data I/O"
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

    let queries = SharedQueryTracker::default();
    queries
        .init(QueryTracker::open(db_root.join("queries")).map_err(|e| e.to_string())?)
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
    Ok(Index { picker, queries })
}

fn path_hash(p: &Path) -> u64 {
    let mut h = DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// Run one search against `idx` and format the results as a single
/// block of text suitable for both the rail and the model history.
fn search(idx: &Index, query: &str, limit: usize) -> Result<String, String> {
    let parser = QueryParser::default();
    let parsed = parser.parse(query);
    let picker_guard = idx
        .picker
        .read()
        .map_err(|e: fff_search::Error| e.to_string())?;
    let picker = picker_guard
        .as_ref()
        .ok_or("fff index handle is empty (scan failed)")?;
    let qt_guard = idx.queries.read().map_err(|e| e.to_string())?;
    let result = picker.fuzzy_search(
        &parsed,
        qt_guard.as_ref(),
        FuzzySearchOptions {
            pagination: PaginationArgs { offset: 0, limit },
            ..Default::default()
        },
    );
    if result.items.is_empty() {
        return Ok(format!(
            "no matches for {query:?} (scanned {} files)",
            result.total_files
        ));
    }
    let mut out = String::new();
    let plural = if result.total_matched == 1 { "" } else { "es" };
    let _ = writeln!(
        out,
        "{} match{plural} (of {} indexed) for {query:?}",
        result.total_matched, result.total_files,
    );
    for item in &result.items {
        let _ = writeln!(out, "{}", item.relative_path(picker));
    }
    Ok(out)
}

impl Tool for FffTool {
    fn name(&self) -> &'static str {
        "fff"
    }

    fn desc(&self) -> &'static str {
        "Fuzzy file-name search (frecency-ranked) over the session's working tree.  \
The first call indexes the directory in the background and blocks until the scan \
settles; subsequent calls share the index and stay live via a filesystem watcher.  \
Use this to locate files by approximate name before reading them with `cat` or \
grepping their contents."
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Fuzzy match — file name, path fragment, or extension.",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT as u64,
                        "description": "Maximum number of results (default 50).",
                    },
                },
                "required": ["query"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Session,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        let args = match parse_args(&input) {
            Ok(a) => a,
            Err(reason) => return invalid_input(id, "fff", "<invalid input>", &reason, emit),
        };
        emit.emit(Kind::ToolCall {
            tool: "fff",
            cmd: args.query.clone(),
            summary: None,
        });
        let cwd = session.cwd();
        let raw = match index_for(&cwd).and_then(|idx| search(idx, &args.query, args.limit)) {
            Ok(s) => s,
            Err(e) => format!("fff error: {e}"),
        };
        let content = clip(&raw, FFF_CAP);
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_query() {
        let a = parse_args(&json!({ "query": "lib.rs" })).unwrap();
        assert_eq!(a.query, "lib.rs");
        assert_eq!(a.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn parse_accepts_limit() {
        let a = parse_args(&json!({ "query": "x", "limit": 10 })).unwrap();
        assert_eq!(a.limit, 10);
    }

    #[test]
    fn parse_clamps_limit() {
        let a = parse_args(&json!({ "query": "x", "limit": 0 })).unwrap();
        assert_eq!(a.limit, 1);
        let a = parse_args(&json!({ "query": "x", "limit": 99_999 })).unwrap();
        assert_eq!(a.limit, MAX_LIMIT);
    }

    #[test]
    fn parse_rejects_missing_query() {
        let e = parse_args(&json!({})).unwrap_err();
        assert!(e.contains("`query`"));
    }

    #[test]
    fn parse_rejects_non_object() {
        let e = parse_args(&json!([])).unwrap_err();
        assert!(e.contains("not a JSON object"));
    }

    #[test]
    fn parse_rejects_limit_wrong_type() {
        let e = parse_args(&json!({ "query": "x", "limit": "ten" })).unwrap_err();
        assert!(e.contains("`limit`"));
    }
}
