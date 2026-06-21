#![allow(clippy::disallowed_methods)]

//! Fail-closed property of the evaluator at **external dispatch**.
//!
//! History: this file used to test fail-closed at *grant-body entry*. The
//! evaluator detected a restrictive grant body and re-execed the whole
//! body into an OS sandbox over IPC; if that confined transport was
//! unavailable, the body errored before it ran. The premise of the
//! original tests was therefore "register no `SANDBOX_SELF`, push a
//! projecting capability frame, and the trivial body never runs". That
//! whole-body re-exec is gone (milestone 4 of
//! `decisions/260617_sandbox-external-children`): a `grant` body now
//! always evaluates **locally** in-process. RAL-owned filesystem effects
//! are checked in process by `capability::check_fs_op`; the surviving
//! confinement boundary is the **per-command sandbox launcher** in
//! `runtime::command::process::build_command`, which confines each
//! external/bundled child it spawns under the effective
//! `SandboxProjection`.
//!
//! So the fail-closed locus moved. Under a restrictive fs grant, an
//! external command that tries to write outside the grant is held by the
//! kernel sandbox (Seatbelt here) when it is spawned — not refused at
//! grant-body entry. These tests assert that new locus: each pairs a
//! positive control (a write *inside* the grant succeeds) with a denial (a
//! write *outside* fails, the file never appears). The positive control is
//! load-bearing: it makes the test fail if confinement were broken in
//! *either* direction — a blanket-deny would fail the control, and a
//! disabled sandbox would let the denied write land.
//!
//! Unlike the old version, this target **imports** `core/tests/common` on
//! purpose: its `#[ctor::ctor]` runs `serve_sandbox_early_init`, which is
//! what lets the per-command re-exec child actually enter Seatbelt and
//! `execve` the target inside it. Without that ctor the re-exec child
//! would land in the libtest framework and crash on the unknown
//! `--sandbox-projection` flag — the command would "fail" for the wrong
//! reason (a broken child, not an enforced policy), which would not prove
//! enforcement at all.
//!
//! Gated to macOS, matching the end-to-end denial tests in
//! `sandbox/launch.rs`: it is the backend that can confine an in-tree
//! re-exec child end-to-end without an external helper binary (`bwrap` on
//! Linux is commonly absent in CI). The *other* fail-closed axis —
//! `projection_enforceable` rejecting `net: false` on a backend with no
//! kernel network enforcement (Windows) — is covered by the unit test
//! `sandbox::tests::projection_enforceable_rejects_net_false_on_windows`
//! and is not re-driven through the eval path here.

#![cfg(all(feature = "test-util", target_os = "macos"))]

mod common;

use ral_core::types::{Break, Capabilities, FsPolicy, Settled, Shell, Value};
use ral_core::{RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin};

/// A `Shell` matching what every front end ends up with after bootstrap:
/// prelude registered, default env, root capabilities.
fn boot() -> Shell {
    ral_core::driver::boot_shell(Default::default(), common::prelude())
}

/// A process-unique work directory under the system temp root, created on
/// the host (outside any sandbox) so a confined child can write into it.
fn unique_workdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ral_fc_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create work dir");
    dir
}

/// A process-unique path *outside* any granted prefix, pre-cleaned so its
/// post-hoc absence is the load-bearing observation.
fn denied_path(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("ral_fc_denied_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// A capability frame whose fs policy confines reads and writes to `dir`.
/// Any `fs` key makes `sandbox_projection()` return `Some(_)`, so the
/// per-command launcher confines every external child.
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

/// Route `src` through the public `run_source_turn` door under `caps`, mirroring
/// exarch's per-tool flow: the turn carries the attenuated capability
/// ceiling in its request and compiles against the live bindings.
fn top_level_under(shell: &mut Shell, caps: Capabilities, src: &str) -> Settled<Value> {
    match shell.run_source_turn(
        src,
        TurnRequest {
            script_name: "<test>",
            caps,
            turn_limit: None,
            detached_limit: None,
            io: TurnIo::Inherit,
            terminal: RequestedTerminalAccess::Denied,
            stdin: TurnStdin::Empty,
            surface: None,
            lifecycle: Box::new(()),
        },
    ) {
        TurnReport::Ran { result, .. } => result,
        TurnReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
    }
}

/// Positive control: under a restrictive fs grant, an external command
/// that writes *inside* the grant's write prefix succeeds and the file
/// lands. Pairs with the denial below — if the sandbox blanket-denied
/// every write this would fail, proving the projection is selective rather
/// than off or all-deny.
#[test]
fn external_write_inside_grant_succeeds() {
    let work = unique_workdir("ctl");
    let work_s = work.to_string_lossy().into_owned();
    let inside = work.join("inside.txt");
    let inside_s = inside.to_string_lossy().into_owned();

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("sh -c 'echo x > {inside_s}'"),
    );
    result.expect("a confined external writing inside the grant must succeed");
    assert!(
        inside.exists(),
        "in-prefix write should have landed at {inside_s}"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// New fail-closed locus at the top level: under a restrictive fs grant,
/// an external command writing *outside* the grant is denied by the
/// per-command sandbox when it spawns. The eval surfaces the child's
/// failure and the file never appears.
#[test]
fn external_write_outside_grant_denied_at_top_level() {
    let work = unique_workdir("top");
    let work_s = work.to_string_lossy().into_owned();
    let denied = denied_path("top");
    let denied_s = denied.to_string_lossy().into_owned();

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("sh -c 'echo x > {denied_s}'"),
    );
    match result {
        Err(Break::Error(_)) => {}
        Err(other) => panic!("expected the confined external to fail, got {other:?}"),
        Ok(v) => panic!(
            "expected fail-closed at external dispatch, got Ok({v:?}); \
             the write outside the grant was not confined"
        ),
    }
    assert!(
        !denied.exists(),
        "out-of-grant write must not have landed at {denied_s}"
    );
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&denied);
}

/// The same denial through the **block boundary**: the projection comes
/// from a `grant [fs: …] { … }` block (not an outer `with_capabilities`),
/// and the external launched inside the forced grant body is confined just
/// the same. This is the surviving analogue of the old block-entry
/// fail-closed test — the grant body runs locally, but the child it spawns
/// is confined.
#[test]
fn external_write_outside_grant_denied_in_block_body() {
    let work = unique_workdir("blk");
    let work_s = work.to_string_lossy().into_owned();
    let denied = denied_path("blk");
    let denied_s = denied.to_string_lossy().into_owned();

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        Capabilities::root(),
        &format!(
            "grant [fs: [read: ['{work_s}'], write: ['{work_s}']]] \
             {{ sh -c 'echo x > {denied_s}' }}"
        ),
    );
    match result {
        Err(Break::Error(_)) => {}
        Err(other) => panic!("expected block-body external to fail closed, got {other:?}"),
        Ok(v) => panic!("expected fail-closed in the grant block body, got Ok({v:?})"),
    }
    assert!(
        !denied.exists(),
        "block-body out-of-grant write must not have landed at {denied_s}"
    );
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&denied);
}

/// When the confined external fails, the top-level turn installs the
/// child's failing status into the parent's mobile, not the previous
/// turn's value. Pins the `finish_top_level` "install the chosen status"
/// step: the parent's `$?` reports the confined failure.
#[test]
fn denied_external_installs_failing_status_into_mobile() {
    let work = unique_workdir("stat");
    let work_s = work.to_string_lossy().into_owned();
    let denied = denied_path("stat");
    let denied_s = denied.to_string_lossy().into_owned();

    let mut shell = boot();
    let result = top_level_under(
        &mut shell,
        restrict_to(&work_s),
        &format!("sh -c 'echo x > {denied_s}'"),
    );
    assert!(
        result.is_err(),
        "expected the confined external to fail closed, got Ok"
    );
    assert_ne!(
        shell.mobile.control.last_status, 0,
        "a denied confined external must install a non-zero last_status into the mobile"
    );
    assert!(!denied.exists());
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&denied);
}

/// A `spawn { … }` worker evaluates its body on a worker thread, but an
/// external it spawns is confined the same way — the per-command launcher
/// folds the same effective projection. A write outside the grant inside
/// the spawned body is denied; `await` surfaces the failure and the side
/// effect never lands.
#[test]
fn external_write_outside_grant_denied_in_spawn_body() {
    let work = unique_workdir("spawn");
    let work_s = work.to_string_lossy().into_owned();
    let denied = denied_path("spawn");
    let denied_s = denied.to_string_lossy().into_owned();

    let mut shell = boot();
    let src = format!(
        "let h = !{{spawn {{ sh -c 'echo x > {denied_s}' }}}}\n\
         let r = await $h\n\
         return $r[value]"
    );
    let result = top_level_under(&mut shell, restrict_to(&work_s), &src);
    match result {
        Err(Break::Error(_)) => {}
        Err(other) => panic!("expected the spawned confined external to fail, got {other:?}"),
        Ok(v) => panic!(
            "expected fail-closed through the spawn worker, got Ok({v:?}); \
             the write outside the grant was not confined on the worker thread"
        ),
    }
    assert!(
        !denied.exists(),
        "spawned out-of-grant write must not have landed at {denied_s}"
    );
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&denied);
}

/// `par` is prelude code over `spawn`, so per-command confinement extends
/// through it: an external inside a `par` task that writes outside the
/// grant is denied, `par` surfaces the failure, and nothing lands.
#[test]
fn external_write_outside_grant_denied_in_par_task() {
    let work = unique_workdir("par");
    let work_s = work.to_string_lossy().into_owned();
    let denied = denied_path("par");
    let denied_s = denied.to_string_lossy().into_owned();

    let mut shell = boot();
    let src = format!("!{{par {{ |x| sh -c 'echo x > {denied_s}'; return $x }} [1] 1}}");
    let result = top_level_under(&mut shell, restrict_to(&work_s), &src);
    assert!(
        result.is_err(),
        "expected par to surface the confined external's failure, got Ok"
    );
    assert!(
        !denied.exists(),
        "par task's out-of-grant write must not have landed at {denied_s}"
    );
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&denied);
}
