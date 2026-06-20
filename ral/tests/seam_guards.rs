#![allow(clippy::disallowed_methods)]

//! Architecture grep guards for the run-turn host-API cutover.
//!
//! [[decisions/260618_run-turn-is-host-api]] makes `Shell::run_turn` /
//! `TurnRequest` / `TurnReport` the only host-facing evaluation seam and
//! collapses the old turn-assembly vocabulary. These source-text scans hold
//! that boundary lexically:
//!
//!   - `ral_core` names no async runtime — tokio never enters core; the seam
//!     is a synchronous `EventSink` taking a `Value`, and the host owns its
//!     concurrency model.
//!   - host crates (`ral`, `exarch`) name none of the internal turn types or
//!     core helpers a host used to assemble a turn by hand (`TurnFrame`,
//!     `IoFrame`, core `TurnOutcome`, `eval_turn`, `arm_lifetime`). Hosts are
//!     request suppliers now.
//!
//! The invariant being guarded is itself lexical — "this name does not occur
//! here" — so a source scan is the right instrument. This test file lives
//! under `tests/`, outside every scanned `src/` tree, so it never matches
//! itself.

use std::path::{Path, PathBuf};

/// The workspace root: the parent of this crate's manifest directory.
///
/// Lifts the compile-time `CARGO_MANIFEST_DIR` literal into a path — the
/// "env-var lifting" adapter site the path-construction discipline in
/// `clippy.toml` exempts. Test scaffolding for a source scan, not a shell
/// path-resolution site, so the named `crate::path` helpers do not apply.
#[allow(clippy::disallowed_methods)]
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate manifest dir has a workspace-root parent")
        .to_path_buf()
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries =
            std::fs::read_dir(&d).unwrap_or_else(|e| panic!("read_dir {}: {e}", d.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    out
}

/// Whether `line` contains `needle` as a whole word — `_` and ASCII
/// alphanumerics are word characters, so `tokio` matches in `tokio::select`
/// (bounded by `:`) but `select` does not match inside `selector`.
fn contains_word(line: &str, needle: &str) -> bool {
    let bytes = line.as_bytes();
    let is_word = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let mut from = 0;
    while let Some(pos) = line[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_word(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Collect `path:line: text` for every line under `<root>/<crate_src>` that
/// satisfies `hit`.
fn scan(crate_src: &str, hit: impl Fn(&str) -> bool) -> Vec<String> {
    let dir = workspace_root().join(crate_src);
    let mut sites = Vec::new();
    for file in rust_files(&dir) {
        let text = std::fs::read_to_string(&file).expect("read source");
        for (n, line) in text.lines().enumerate() {
            if hit(line) {
                sites.push(format!("  {}:{}: {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    sites
}

/// Assert no `.rs` file under `<root>/<crate_src>` names `needle` as a whole
/// word; on failure, list the offending sites.
fn assert_absent(crate_src: &str, needle: &str) {
    let sites = scan(crate_src, |line| contains_word(line, needle));
    assert!(
        sites.is_empty(),
        "`{needle}` must not appear in `{crate_src}` (run-turn-is-host-api cutover):\n{}",
        sites.join("\n"),
    );
}

/// The host-loop ADR's invariant: tokio never enters `ral_core`. The seam is
/// a synchronous `EventSink`; the host owns the runtime. `tokio` is the
/// decisive token — banning it transitively bans `tokio::select!`,
/// `tokio::sync::mpsc`, and `tokio::task::spawn_blocking`, the three the ADR
/// lists. Bare `mpsc`/`select` are *not* banned: core's structured
/// concurrency (`spawn`/`await`) is built on `std::sync::mpsc`, and "select"
/// occurs in sandbox path names — neither is the async runtime.
#[test]
fn ral_core_names_no_async_runtime() {
    assert_absent("core/src", "tokio");
    assert_absent("core/src", "spawn_blocking");
}

/// Hosts are request suppliers: they build a `TurnRequest`, call `run_turn`,
/// and render a `TurnReport`. None of the collapsed internal turn types, nor
/// the core helpers a host once used to assemble a turn, may reappear.
#[test]
fn hosts_name_no_turn_assembly_vocabulary() {
    for host in ["ral/src", "exarch/src"] {
        assert_absent(host, "TurnFrame");
        assert_absent(host, "IoFrame");
        assert_absent(host, "eval_turn");
        assert_absent(host, "arm_lifetime");

        // exarch legitimately owns `session::TurnOutcome` (provider-message
        // outcomes) — a different layer the ADR keeps. Forbid only a host
        // naming *core's* `TurnOutcome`, i.e. `TurnOutcome` reached through
        // `ral_core`.
        let sites = scan(host, |line| {
            line.contains("TurnOutcome") && line.contains("ral_core")
        });
        assert!(
            sites.is_empty(),
            "host `{host}` must not name core's `TurnOutcome` (collapsed into `TurnReport`):\n{}",
            sites.join("\n"),
        );
    }
}
