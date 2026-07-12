#![allow(clippy::disallowed_methods)]
#![cfg(unix)]

//! Negative acceptance tests for the sandbox-external-children safety
//! invariant (`decisions/260617_sandbox-external-children.md`,
//! §"Safety invariant").
//!
//! The invariant says there is *no third path* for filesystem effects:
//! every RAL-owned effect is checked by `capability::check_fs_op` on the
//! canonical path *before* the syscall.  These tests prove that for the
//! gaps the existing `eval_fuzz.rs` denied-fs suite does not already cover:
//!
//!   * the RAL-owned **write** edge (stdout/append redirect — there is no
//!     structured `write-file` builtin; cp/mkdir/mv/rm are bundled tools
//!     and are *child-owned*, so they are not exercised here);
//!   * **module loading** (`source` / `use`) of a `.ral` file outside the
//!     read set;
//!   * the **stdin (`<`)** and **stderr (`2>`)** redirect opens (the
//!     `>` stdout case is already `grant_fs_write_denies_external_redirect`);
//!   * the **pipeline helper transport** — a ral closure stage runs in a
//!     re-exec'd helper carrying the same grant stack, so its in-process
//!     redirect open hits `check_fs_op` exactly as in the parent.
//!
//! Each negative test runs a restrictive `grant [fs: …]` whose body
//! performs a RAL-owned fs op on a path *outside* the granted set, asserts
//! the script fails, and asserts the body effect never landed (no file
//! created / read / written).  Positive controls inside the granted set
//! distinguish enforcement from blanket failure.

mod common;

use ral_core::builtins;
use ral_core::{
    Break, Error, Shell, Value, elaborator::elaborate, evaluator::evaluate, syntax::parser::parse,
    typecheck,
};

/// Evaluate a ral script through the public API, exactly as `eval_fuzz.rs`
/// does: parse, elaborate, typecheck against the prelude schemes (so the
/// evaluator's mode wires are written), then evaluate in a fresh shell.
fn eval(input: &str) -> ral_core::types::Settled<Value> {
    let ast = parse(input).map_err(|e: ral_core::syntax::parser::ParseError| {
        Break::Error(Error::new(e.to_string(), 2))
    })?;
    let comp = elaborate(&ast, std::collections::HashSet::default());
    let comp = match typecheck(
        &comp,
        ral_core::SessionSchemes::from_schemes(common::prelude_schemes()),
    ) {
        Ok(annotated) => std::sync::Arc::new(annotated),
        Err(errors) => {
            let msg = errors
                .iter()
                .map(|e| e.kind.render_message())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Break::Error(Error::new(format!("type error: {msg}"), 2)));
        }
    };
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    builtins::register(&mut shell, common::prelude_comp());
    evaluate(&comp, &mut shell)
}

fn must_succeed(input: &str) -> Value {
    eval(input).unwrap_or_else(|e| panic!("should succeed: {input:?}\n  error: {e:?}"))
}

/// Assert a script fails *and* that the failure is the capability gate, not
/// some incidental error.  `check_fs_op` renders `fs read denied by grant`
/// / `fs write denied by grant`; asserting on that substring rules out a
/// parse error, a missing-command error, or a spurious io error masquerading
/// as enforcement.
fn must_deny(input: &str) {
    match eval(input) {
        Ok(v) => panic!("should be denied at check_fs_op: {input:?}\n  but succeeded: {v:?}"),
        Err(Break::Error(err)) => assert!(
            err.message.contains("denied by grant"),
            "expected an fs-grant denial, got a different error: {:?}\n  script: {input:?}",
            err.message
        ),
        Err(other) => panic!("expected an fs-grant denial, got: {other:?}\n  script: {input:?}"),
    }
}

/// A fresh, unique scratch directory under the system temp dir, keyed on the
/// pid so parallel test binaries do not collide.  Returns the directory; the
/// caller cleans it up.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ral-denyfs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ── 1. RAL-owned writes (redirect edge) ──────────────────────────────────
//
// There is no structured `write-file` / `mkdir` / `rm` / `cp` / `mv`
// *builtin* in the language: the RAL-owned write surface is the redirect
// (`>`, `>>`), opened in-process by `runtime::command::open_file` →
// `check_fs_write`.  The bundled `cp`/`mkdir`/`mv`/`rm` from uutils are
// command images (child-owned), not in-process RAL effects, so they are
// out of scope for the *RAL-owned* bucket.  These tests pin the redirect
// write edge for the gaps the existing external `>` test does not name.

/// A `>` stdout redirect to a path outside the write set is rejected before
/// the file is created.  Distinct from `grant_fs_write_denies_external_redirect`
/// (external `/bin/echo`): here the producer is a builtin whose bytes flow
/// through the same in-process `open_file` gate, proving the gate is on the
/// redirect open, not on the command kind.
#[cfg(unix)]
#[test]
fn grant_fs_write_denies_builtin_redirect() {
    let dir = scratch("wbuiltin");
    let allowed = dir.join("allowed");
    let denied = dir.join("denied.txt");
    let script = format!(
        "grant [fs: [write: ['{}']]] {{ to-string 'leak' > '{}' }}",
        allowed.display(),
        denied.display()
    );
    must_deny(&script);
    assert!(
        !denied.exists(),
        "denied write target must never be created"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Positive control for the write edge: the same redirect to a path *inside*
/// the write set succeeds and the file lands.  This proves the denial above
/// is enforcement of the granted region, not a blanket "grant blocks all
/// redirects" failure.
#[cfg(unix)]
#[test]
fn grant_fs_write_allows_redirect_inside_set() {
    let dir = scratch("wallow");
    let target = dir.join("ok.txt");
    let script = format!(
        "grant [fs: [write: ['{}']]] {{ to-string 'kept' > '{}' }}",
        dir.display(),
        target.display()
    );
    must_succeed(&script);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "kept",
        "granted write must land"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `>>` append to a denied path is rejected too — append takes the same
/// `check_fs_write` arm in `open_file`, so a pre-existing file is never
/// touched and a missing one is never created.
#[cfg(unix)]
#[test]
fn grant_fs_write_denies_append_redirect() {
    let dir = scratch("wappend");
    let allowed = dir.join("allowed");
    let denied = dir.join("log.txt");
    std::fs::write(&denied, "original").unwrap();
    let script = format!(
        "grant [fs: [write: ['{}']]] {{ to-string 'more' >> '{}' }}",
        allowed.display(),
        denied.display()
    );
    must_deny(&script);
    assert_eq!(
        std::fs::read_to_string(&denied).unwrap(),
        "original",
        "denied append must not modify the file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 2. Module / source loading ───────────────────────────────────────────
//
// `source <file>` and `use <file>` read the `.ral` text through
// `modules::read_and_normalize` → `check_fs_read`.  A load of a file outside
// the read set must fail at that gate, before the bytes are read or the
// module body runs.  (Only the direct-path case is covered; RAL_PATH
// discovery is separately tracked.)

/// `source` of a `.ral` file outside the read set is denied at `check_fs_read`.
/// The sourced file performs a side effect (a redirect into a writable temp)
/// so that, if the gate were bypassed, the effect would be observable on
/// disk; the assertion that the effect file is absent proves the body never
/// ran.
#[cfg(unix)]
#[test]
fn grant_fs_read_denies_source_outside_set() {
    let dir = scratch("source");
    let allowed = dir.join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let module = dir.join("mod.ral");
    let witness = dir.join("witness.txt");
    // If the module body ever runs it would write `witness.txt`; it must not.
    std::fs::write(
        &module,
        format!("to-string 'ran' > '{}'\n", witness.display()),
    )
    .unwrap();
    let script = format!(
        "grant [fs: [read: ['{}'], write: ['{}']]] {{ source '{}' }}",
        allowed.display(),
        dir.display(),
        module.display()
    );
    must_deny(&script);
    assert!(
        !witness.exists(),
        "denied source must not run its body (no witness file)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `use` of a `.ral` module outside the read set is denied at the same gate.
#[cfg(unix)]
#[test]
fn grant_fs_read_denies_use_outside_set() {
    let dir = scratch("use");
    let allowed = dir.join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let module = dir.join("lib.ral");
    std::fs::write(&module, "let answer = 42\n").unwrap();
    let script = format!(
        "grant [fs: [read: ['{}']]] {{ use '{}' }}",
        allowed.display(),
        module.display()
    );
    must_deny(&script);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Positive control for module loading: `source` of a file *inside* the read
/// set runs, and its body effect lands inside the write set.  This proves the
/// denials above gate on the region, not on `source`/`use` per se.
#[cfg(unix)]
#[test]
fn grant_fs_read_allows_source_inside_set() {
    let dir = scratch("sourceok");
    let module = dir.join("mod.ral");
    let witness = dir.join("witness.txt");
    std::fs::write(
        &module,
        format!("to-string 'ran' > '{}'\n", witness.display()),
    )
    .unwrap();
    let script = format!(
        "grant [fs: [read: ['{}'], write: ['{}']]] {{ source '{}' }}",
        dir.display(),
        dir.display(),
        module.display()
    );
    must_succeed(&script);
    assert_eq!(
        std::fs::read_to_string(&witness).unwrap(),
        "ran",
        "granted source body must run and its effect must land"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 3. Redirects: stdin (`<`) and stderr (`2>`) ──────────────────────────
//
// The parent opens every redirect file through `open_file` → `check_fs_op`
// *before* the command spawns, so the file open is the thing under test.  A
// bundled `head` is the command so the redirect-file open — not the command
// — is what trips the gate.  (`>` stdout is already covered by
// `grant_fs_write_denies_external_redirect`.)

/// `< /denied` stdin redirect is denied at `check_fs_read`: the parent tries
/// to open the input file before spawning `head`, and the open is gated.
#[cfg(unix)]
#[test]
fn grant_fs_read_denies_stdin_redirect() {
    let dir = scratch("stdin");
    let allowed = dir.join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, "top secret\n").unwrap();
    let script = format!(
        "grant [fs: [read: ['{}']]] {{ head < '{}' }}",
        allowed.display(),
        secret.display()
    );
    must_deny(&script);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `2> /denied` stderr redirect is denied at `check_fs_write`: the parent
/// tries to open the stderr target file before spawning the command, and the
/// open is gated, so the file is never created.
#[cfg(unix)]
#[test]
fn grant_fs_write_denies_stderr_redirect() {
    let dir = scratch("stderr");
    let allowed = dir.join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let denied = dir.join("err.log");
    let script = format!(
        "grant [fs: [write: ['{}']]] {{ head '/dev/null' 2> '{}' }}",
        allowed.display(),
        denied.display()
    );
    must_deny(&script);
    assert!(
        !denied.exists(),
        "denied stderr target must never be created"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Positive control for redirects: a `<` stdin redirect from a file *inside*
/// the read set succeeds, proving the gate is on the region, not on the
/// presence of a redirect.
#[cfg(unix)]
#[test]
fn grant_fs_read_allows_stdin_redirect_inside_set() {
    let dir = scratch("stdinok");
    let input = dir.join("in.txt");
    std::fs::write(&input, "line one\nline two\n").unwrap();
    let script = format!(
        "grant [fs: [read: ['{}']]] {{ from-string < '{}' }}",
        dir.display(),
        input.display()
    );
    let out = must_succeed(&script);
    assert_eq!(
        out,
        Value::String("line one\nline two\n".into()),
        "granted stdin redirect must read the file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 4. Pipeline helper transport ─────────────────────────────────────────
//
// A byte-pipeline stage that is ral code (`{ |s| … }`) runs in a re-exec'd
// helper subprocess that carries the parent's grant stack on its `WireMobile`
// snapshot (`Capabilities` serialise the grants across the helper protocol).
// A RAL-owned fs effect inside that stage — here a redirect, the only
// in-process write edge — is therefore checked by the helper's own
// `check_fs_op` exactly as in the parent.  This proves the helper is *not* a
// third, unchecked filesystem path.  The stage takes a value edge
// (`from-string` decodes the seed bytes to a String the closure binds as
// `s`); the redirect inside the closure is the in-process write under test.

/// A helper-staged closure that redirects into a path outside the write set
/// is denied at `check_fs_op` inside the helper.  The seed bytes reach the
/// stage (so the helper genuinely ran), but the denied redirect target is
/// never created.
#[cfg(unix)]
#[test]
fn grant_fs_write_denies_helper_stage_redirect() {
    let dir = scratch("helper");
    let allowed = dir.join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let denied = dir.join("leak.txt");
    let script = format!(
        "grant [fs: [write: ['{}']]] {{ echo seed | from-string | {{ |s| /bin/echo $s > '{}' }} }}",
        allowed.display(),
        denied.display()
    );
    must_deny(&script);
    assert!(
        !denied.exists(),
        "helper stage's denied redirect must never create the file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Positive control for the helper transport: the same closure stage
/// redirecting into a path *inside* the write set succeeds and the file
/// lands.  This proves the helper enforces the granted region (carries the
/// stack correctly) rather than failing on every redirect.
#[cfg(unix)]
#[test]
fn grant_fs_write_allows_helper_stage_redirect_inside_set() {
    let dir = scratch("helperok");
    let target = dir.join("kept.txt");
    let script = format!(
        "grant [fs: [write: ['{}']]] {{ echo seed | from-string | {{ |s| /bin/echo $s > '{}' }} }}",
        dir.display(),
        target.display()
    );
    must_succeed(&script);
    assert!(
        std::fs::read_to_string(&target).unwrap().contains("seed"),
        "granted helper stage redirect must land the seed bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 5. Read-gated query builtins (`file-info`, `resolve-path`) ────────────
//
// `file-info` and `resolve-path` are RAL-owned query builtins that stat /
// canonicalise their path argument.  Both run `check_fs_read` on the
// resolved argument *before* touching the filesystem (`file-info` before
// `symlink_metadata`, `resolve-path` before `canonicalise_strict`), so a
// path outside the read set is denied at `check_fs_op`, not by an io error.
// Because the gate fires first, the negative cases need no file on disk —
// `must_deny`'s "denied by grant" assertion rules out an io "not found"
// masquerading as enforcement.

/// `file-info` on a path outside the read set is denied at `check_fs_read`,
/// before `symlink_metadata` — so the denial fires whether or not the path
/// exists.  Asserting on the grant-denial message (not an io error)
/// proves the gate, not the missing file, rejected the call.
#[cfg(unix)]
#[test]
fn grant_fs_read_denies_file_info_outside_set() {
    let dir = scratch("fileinfo");
    let allowed = dir.join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let denied = dir.join("secret.txt");
    std::fs::write(&denied, "top secret\n").unwrap();
    let script = format!(
        "grant [fs: [read: ['{}']]] {{ file-info '{}' }}",
        allowed.display(),
        denied.display()
    );
    must_deny(&script);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Positive control for `file-info`: under a grant that *does* permit the
/// path, the stat runs and returns the file's metadata map.  This proves
/// the denial above gates on the region, not on `file-info` per se.
#[cfg(unix)]
#[test]
fn grant_fs_read_allows_file_info_inside_set() {
    let dir = scratch("fileinfook");
    let target = dir.join("seen.txt");
    std::fs::write(&target, "hello\n").unwrap();
    let script = format!(
        "grant [fs: [read: ['{}']]] {{ file-info '{}' }}",
        dir.display(),
        target.display()
    );
    let out = must_succeed(&script);
    match out {
        Value::Map(m) => assert_eq!(
            m.get("name"),
            Some(&Value::String("seen.txt".into())),
            "granted file-info must stat the file and report its name"
        ),
        other => panic!("file-info should return a Map, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `resolve-path` on a path outside the read set is denied at
/// `check_fs_read`, before `canonicalise_strict` — so the denial fires
/// whether or not the path exists.  The grant-denial message (not an io
/// error) proves the gate rejected the call.
#[cfg(unix)]
#[test]
fn grant_fs_read_denies_resolve_path_outside_set() {
    let dir = scratch("resolve");
    let allowed = dir.join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let denied = dir.join("target");
    std::fs::create_dir_all(&denied).unwrap();
    let script = format!(
        "grant [fs: [read: ['{}']]] {{ resolve-path '{}' }}",
        allowed.display(),
        denied.display()
    );
    must_deny(&script);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Positive control for `resolve-path`: under a grant that permits the
/// path, the canonicalisation runs and returns an absolute path.  This
/// proves the denial above gates on the region, not on `resolve-path`
/// per se.
#[cfg(unix)]
#[test]
fn grant_fs_read_allows_resolve_path_inside_set() {
    let dir = scratch("resolveok");
    let target = dir.join("here");
    std::fs::create_dir_all(&target).unwrap();
    let script = format!(
        "grant [fs: [read: ['{}']]] {{ resolve-path '{}' }}",
        dir.display(),
        target.display()
    );
    let out = must_succeed(&script);
    match out {
        Value::String(s) => assert!(
            s.starts_with('/'),
            "granted resolve-path must return an absolute path, got {s:?}"
        ),
        other => panic!("resolve-path should return a String, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 6. Write-gated temp-path builtins (`temp-dir`, `temp-file`) ───────────
//
// `temp-dir` and `temp-file` take no path argument: they create a new entry
// under `std::env::temp_dir()` and run `check_fs_write` on that system temp
// directory *before* creating anything.  A grant whose write set is a
// strict subdirectory of the temp root therefore does NOT cover the temp
// root itself (`path_within` is prefix containment, and the root is the
// parent of the granted subdir), so the create is denied at
// `check_fs_op`.  The positive control grants write to the temp root,
// which does cover it.

/// `temp-dir` under a grant whose write set is a strict subdirectory of the
/// system temp root is denied at `check_fs_write` on `std::env::temp_dir()`,
/// before any directory is created.
#[cfg(unix)]
#[test]
fn grant_fs_write_denies_temp_dir_outside_set() {
    let dir = scratch("tmpdir");
    let script = format!("grant [fs: [write: ['{}']]] {{ temp-dir }}", dir.display());
    must_deny(&script);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Positive control for `temp-dir`: under a grant that permits the system
/// temp root, the create runs and returns a path.  This proves the denial
/// above gates on the region, not on `temp-dir` per se.  The returned
/// directory is cleaned up.
#[cfg(unix)]
#[test]
fn grant_fs_write_allows_temp_dir_inside_set() {
    let root = std::env::temp_dir();
    let script = format!("grant [fs: [write: ['{}']]] {{ temp-dir }}", root.display());
    let out = must_succeed(&script);
    match out {
        Value::String(s) => {
            assert!(
                std::path::Path::new(&s).is_dir(),
                "granted temp-dir must create and return a directory, got {s:?}"
            );
            let _ = std::fs::remove_dir_all(&s);
        }
        other => panic!("temp-dir should return a String, got {other:?}"),
    }
}

/// `temp-file` under a grant whose write set is a strict subdirectory of the
/// system temp root is denied at `check_fs_write` on `std::env::temp_dir()`,
/// before any file is created.
#[cfg(unix)]
#[test]
fn grant_fs_write_denies_temp_file_outside_set() {
    let dir = scratch("tmpfile");
    let script = format!("grant [fs: [write: ['{}']]] {{ temp-file }}", dir.display());
    must_deny(&script);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Positive control for `temp-file`: under a grant that permits the system
/// temp root, the create runs and returns a path.  This proves the denial
/// above gates on the region, not on `temp-file` per se.  The returned
/// file is cleaned up.
#[cfg(unix)]
#[test]
fn grant_fs_write_allows_temp_file_inside_set() {
    let root = std::env::temp_dir();
    let script = format!(
        "grant [fs: [write: ['{}']]] {{ temp-file }}",
        root.display()
    );
    let out = must_succeed(&script);
    match out {
        Value::String(s) => {
            assert!(
                std::path::Path::new(&s).is_file(),
                "granted temp-file must create and return a file, got {s:?}"
            );
            let _ = std::fs::remove_file(&s);
        }
        other => panic!("temp-file should return a String, got {other:?}"),
    }
}
