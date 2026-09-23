#![allow(clippy::disallowed_methods)]

//! Fail-closed at **external dispatch**: a grant body evaluates in process,
//! and what confines the commands it spawns is the per-command launcher
//! (`runtime::command::process::build_command`), so a child writing outside
//! the grant is held by Seatbelt at spawn.  Each test pairs a positive control
//! (a write inside the grant lands) with a denial (one outside never appears);
//! the control is load-bearing — a blanket deny would fail it, a disabled
//! sandbox would let the denied write land.
//!
//! Imports `common` for its `#[ctor]`, which runs `serve_sandbox_early_init`
//! so the re-exec child enters Seatbelt and `execve`s the target rather than
//! landing in libtest and dying on `--sandbox-projection` — a failure for the
//! wrong reason.  macOS-only: the one backend that confines an in-tree re-exec
//! end-to-end without a helper binary (`bwrap` is often absent in CI).
//! `projection_enforceable`'s own fail-closed axis is unit-tested in `sandbox`.

#![cfg(all(feature = "test-util", target_os = "macos"))]

mod common;

use ral_core::path::NormalizedPrefix;
use ral_core::protocol::{Program, Run};
use ral_core::types::{Break, Capabilities, FsPolicy, GrantStack, Settled, Shell, Value};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};

/// A `Shell` matching what every front end ends up with after bootstrap:
/// prelude registered, default env, root capabilities.
fn boot() -> Shell {
    ral_core::boot::boot_shell(
        ral_core::io::TerminalState::default(),
        common::prelude(),
        &ral_core::boot::HostSurface::default(),
    )
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
            read_prefixes: vec![NormalizedPrefix::from_surface(dir)],
            write_prefixes: vec![NormalizedPrefix::from_surface(dir)],
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
            caps: GrantStack::of(caps),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
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
/// the same. The grant body runs locally, but the child it spawns
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

/// When the confined external fails, its status is the failure's own exit
/// code, not the previous run's value: a denied write fails closed, and the
/// outcome's status reports it.
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
        ral_core::run::status(&result),
        0,
        "a denied confined external must report a non-zero status"
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
