#![allow(clippy::disallowed_methods)]

//! Negative coverage for the **child-owned** filesystem bucket of the
//! safety invariant in
//! `decisions/260617_sandbox-external-children` §"Safety invariant".
//!
//! Under an fs-restricting `grant`, every filesystem effect is either
//! ral-owned (checked in process by `capability::check_fs_op`),
//! child-owned (launched as a process under the effective
//! `SandboxProjection`), or outside the grant effect surface. This file
//! drives the **child-owned** path end-to-end through the public eval
//! API and proves the kernel sandbox denies child filesystem effects
//! that fall outside the grant:
//!
//!   1. a host external that *reads* outside the read set is denied;
//!   2. a bundled coreutil that *writes* outside the write set is denied;
//!   3. a bundled coreutil used as a **byte pipeline stage** that writes
//!      outside the write set is denied (the key gap — bundled byte
//!      stages are direct `ExecImage::BundledTool` children that must
//!      receive the same per-command sandbox as any external);
//!   4. the same denial for a *downstream* stage of a multi-stage
//!      bundled byte pipeline.
//!
//! Each denial is paired with an in-grant **positive control** that does
//! land its effect, so the test proves *selective* enforcement: a
//! blanket-deny would fail the control, and a disabled sandbox would let
//! the denied effect through. The controls are also load-bearing against
//! a vacuous "denied because the command never ran" — they confirm the
//! same command shape, redirected inside the grant, actually executes and
//! touches the filesystem.
//!
//! These complement `core/tests/sandbox_fail_closed.rs`, which already
//! covers host-external *write* denial outside an fs grant at the top
//! level, in a `grant { … }` block body, in a `spawn` body, and in a
//! `par` task. The lib-level seam tests
//! `sandbox::launch::tests::{external_denied_write_outside_fs_grant,
//! bundled_tool_denied_outside_fs_grant}` cover the `sandboxed_command`
//! seam directly; this file proves the same enforcement *through eval*.
//!
//! Like `sandbox_fail_closed.rs`, this target imports `core/tests/common`
//! so its `#[ctor::ctor]` runs `serve_sandbox_early_init` — that is what
//! lets a per-command re-exec child actually enter Seatbelt and run the
//! confined target. Without it the re-exec child would land in the
//! libtest framework and crash on the unknown `--sandbox-projection`
//! flag, "failing" for the wrong reason and proving nothing.
//!
//! Gated to macOS, matching `sandbox_fail_closed.rs` and the end-to-end
//! denial tests in `sandbox/launch.rs`: macOS (Seatbelt) is the backend
//! that can confine an in-tree re-exec child end-to-end without an
//! external helper binary (`bwrap` on Linux is commonly absent in CI;
//! Windows is fail-closed and rejected by `projection_enforceable`).

#![cfg(all(feature = "test-util", feature = "coreutils", target_os = "macos"))]

mod common;

use ral_core::transport::{Program, Run};
use ral_core::types::{Break, Capabilities, FsPolicy, Settled, Shell, Value};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};

/// A `Shell` matching what every front end ends up with after bootstrap:
/// prelude registered, default env, root capabilities.
fn boot() -> Shell {
    ral_core::driver::boot_shell(
        ral_core::io::TerminalState::default(),
        common::prelude(),
        &ral_core::driver::HostSurface::default(),
    )
}

/// A process-unique work directory under the system temp root, created on
/// the host (outside any sandbox) so a confined child can read/write into
/// it. Returns `(dir, dir_as_string)`.
fn unique_workdir(tag: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("ral_dfc_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create work dir");
    let s = dir.to_string_lossy().into_owned();
    (dir, s)
}

/// A process-unique path *outside* any granted prefix, pre-cleaned so its
/// post-hoc absence is the load-bearing observation. Returns
/// `(path, path_as_string)`.
fn denied_path(tag: &str) -> (std::path::PathBuf, String) {
    let p = std::env::temp_dir().join(format!("ral_dfc_denied_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_dir_all(&p);
    let s = p.to_string_lossy().into_owned();
    (p, s)
}

/// A capability frame whose fs policy confines reads and writes to `dir`.
/// Any `fs` key makes `sandbox_projection()` return `Some(_)`, so the
/// per-command launcher confines every external/bundled child.
fn restrict_to(dir: &str) -> Capabilities {
    Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![dir.into()],
            write_prefixes: vec![dir.into()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    }
}

/// Route `src` through the public `run` door under `caps`, mirroring
/// exarch's per-tool flow: the run carries the attenuated capability
/// ceiling in its request and compiles against the live bindings.
fn top_level_under(shell: &mut Shell, caps: Capabilities, src: &str) -> Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(src.into()),
            script_name: "<test>".into(),
            caps,
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { result, .. } => result,
        RunReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
    }
}

/// Assert that a confined child failed closed (the eval surfaced an
/// error) rather than slipping through with `Ok`.
fn assert_denied(result: &ral_core::types::Settled<Value>, what: &str) {
    match result {
        Err(Break::Error(_)) => {}
        Err(other) => panic!("expected {what} to fail closed, got {other:?}"),
        Ok(v) => panic!(
            "expected fail-closed for {what}, got Ok({v:?}); \
             the out-of-grant child effect was NOT confined — enforcement hole"
        ),
    }
}

// ── 1. Host external READ denial ─────────────────────────────────────────

/// Positive control for the read test: a host external (`/bin/cat`)
/// reading a file *inside* the grant's read prefix succeeds and returns
/// the file's bytes — proving the projection grants the read it lists.
#[test]
fn external_read_inside_grant_succeeds() {
    let (work, work_s) = unique_workdir("readctl");
    let inside = work.join("secret.txt");
    let inside_s = inside.to_string_lossy().into_owned();
    std::fs::write(&inside, "PLAINTEXT\n").expect("seed in-grant file");

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("let s = !{{/bin/cat '{inside_s}' | from-string}}\nreturn $s"),
    );
    let out = result.expect("a confined external reading inside the grant must succeed");
    assert_eq!(
        out,
        Value::String("PLAINTEXT\n".into()),
        "in-prefix read should have returned the file's bytes"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// Under a grant whose read set is `work` (which does NOT contain the
/// secret), a host external that reads the secret outside the read set is
/// denied by the per-command sandbox: the child fails and no data leaks.
/// The secret file lives outside the granted prefix and is created on the
/// host so its content exists to be (not) leaked.
#[test]
fn external_read_outside_grant_denied() {
    let (work, work_s) = unique_workdir("readdeny");
    // The secret lives at a sibling temp path *outside* `work`.
    let (secret, secret_s) = denied_path("readsecret");
    std::fs::write(&secret, "TOPSECRET\n").expect("seed out-of-grant secret");

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        // Capture the read through `from-string`: if the read were
        // permitted, the secret bytes would flow back as the value.
        &format!("let s = !{{/bin/cat '{secret_s}' | from-string}}\nreturn $s"),
    );

    assert_denied(&result, "a host external reading outside the read set");
    if let Ok(v) = &result {
        let leaked = format!("{v:?}");
        assert!(
            !leaked.contains("TOPSECRET"),
            "the out-of-grant secret leaked back through the pipeline: {leaked}"
        );
    }

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&secret);
}

// ── 2. Bundled command WRITE denial (end-to-end through eval) ────────────

/// Positive control: a bundled coreutil (`touch`) creating a file *inside*
/// the grant's write prefix succeeds and the file lands. Pairs with the
/// denial below to prove selective enforcement of the bundled-tool child
/// placement through eval (the lib seam test covers the same at the
/// `sandboxed_command` layer).
#[test]
fn bundled_write_inside_grant_succeeds() {
    let (work, work_s) = unique_workdir("bunctl");
    let inside = work.join("made.txt");
    let inside_s = inside.to_string_lossy().into_owned();

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("touch '{inside_s}'"),
    );
    result.expect("a confined bundled `touch` inside the grant must succeed");
    assert!(
        inside.exists(),
        "in-prefix bundled write should have landed at {inside_s}"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// Under a restrictive fs grant, a bundled coreutil (`touch`) writing
/// *outside* the write set is denied by the per-command sandbox when the
/// `--ral-bundled-tool` child is launched. The eval surfaces the failure
/// and the file never appears.
#[test]
fn bundled_write_outside_grant_denied() {
    let (work, work_s) = unique_workdir("bundeny");
    let (denied, denied_s) = denied_path("bunwrite");

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("touch '{denied_s}'"),
    );

    assert_denied(&result, "a bundled `touch` writing outside the write set");
    assert!(
        !denied.exists(),
        "out-of-grant bundled write must not have landed at {denied_s}"
    );

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&denied);
}

// ── 3. Bundled BYTE PIPELINE stage WRITE denial (the key gap) ────────────

/// Positive control for the byte-pipeline test: a bundled byte stage
/// (`tee`) that writes its input to a path *inside* the grant lands the
/// file. `tee` is a bundled coreutil that copies stdin to both stdout and
/// the named file; here it runs as a direct `ExecImage::BundledTool`
/// pipeline stage, confined to the grant, and its write inside the prefix
/// succeeds.
#[test]
fn bundled_pipeline_stage_write_inside_grant_succeeds() {
    let (work, work_s) = unique_workdir("teectl");
    let inside = work.join("teed.txt");
    let inside_s = inside.to_string_lossy().into_owned();

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("printf 'x\\n' | tee '{inside_s}'"),
    );
    result.expect("a confined bundled `tee` stage writing inside the grant must succeed");
    assert!(
        inside.exists(),
        "in-prefix bundled byte-stage write should have landed at {inside_s}"
    );
    assert_eq!(
        std::fs::read_to_string(&inside).unwrap_or_default(),
        "x\n",
        "the tee'd file should contain the piped bytes"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// THE KEY GAP: a bundled coreutil used as a byte pipeline stage that
/// writes *outside* the grant must be denied by the per-command sandbox.
/// `printf 'x\n' | tee /denied/out` runs `tee` as a direct bundled child;
/// its write to the out-of-grant path is held by Seatbelt, the pipeline
/// fails, and the file never lands.
#[test]
fn bundled_pipeline_stage_write_outside_grant_denied() {
    let (work, work_s) = unique_workdir("teedeny");
    let (denied, denied_s) = denied_path("teewrite");

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("printf 'x\\n' | tee '{denied_s}'"),
    );

    assert_denied(
        &result,
        "a bundled `tee` byte stage writing outside the write set",
    );
    assert!(
        !denied.exists(),
        "out-of-grant bundled byte-stage write must not have landed at {denied_s}"
    );

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&denied);
}

// ── 4. Multi-stage bundled byte pipeline: downstream stage denial ────────

/// Positive control for the multi-stage test: in a three-stage bundled
/// byte pipeline, a downstream `tee` writing *inside* the grant lands the
/// file. `printf … | cat | tee INSIDE` — every stage is a direct bundled
/// child confined to the grant; the downstream write inside the prefix
/// succeeds.
#[test]
fn multistage_pipeline_downstream_write_inside_grant_succeeds() {
    let (work, work_s) = unique_workdir("multictl");
    let inside = work.join("multi.txt");
    let inside_s = inside.to_string_lossy().into_owned();

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("printf 'y\\n' | cat | tee '{inside_s}'"),
    );
    result.expect("a confined downstream bundled `tee` writing inside the grant must succeed");
    assert!(
        inside.exists(),
        "downstream in-prefix write should have landed at {inside_s}"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// A downstream stage of a multi-stage bundled byte pipeline that writes
/// outside the grant is denied. `printf … | cat | tee /denied/out`: the
/// upstream `printf`/`cat` stages produce bytes, but the downstream `tee`
/// (a direct bundled child) is confined by the projection and its
/// out-of-grant write is held by Seatbelt; the pipeline fails and the
/// file never lands.
#[test]
fn multistage_pipeline_downstream_write_outside_grant_denied() {
    let (work, work_s) = unique_workdir("multideny");
    let (denied, denied_s) = denied_path("multiwrite");

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("printf 'y\\n' | cat | tee '{denied_s}'"),
    );

    assert_denied(
        &result,
        "a downstream bundled `tee` stage writing outside the write set",
    );
    assert!(
        !denied.exists(),
        "downstream out-of-grant write must not have landed at {denied_s}"
    );

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&denied);
}
