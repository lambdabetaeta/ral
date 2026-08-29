#![allow(clippy::disallowed_methods)]

//! A confined child never receives a dynamic-loader override.
//!
//! `runtime::command::process::apply_env` strips the loader hooks under an
//! active grant, because a loader hook makes an admitted binary run someone
//! else's code and the grant's judgment about *which* program may run would
//! mean nothing.  On macOS the strip is load-bearing twice over: every
//! confined child is a re-exec of ral itself, and dyld acts on
//! `DYLD_INSERT_LIBRARIES` before `main` — so an injected dylib would run in
//! the trampoline *before* it enters Seatbelt, outside the sandbox the author
//! asked for.
//!
//! The probe is the bundled `printenv`, whose confined form is exactly that
//! re-exec trampoline (a host `/bin/*` would prove nothing: dyld strips the
//! variables from a SIP-restricted process itself).  A second, non-loader
//! variable set in the same `within` is the positive control: it must arrive,
//! or the test would pass on env plumbing that never worked.
//!
//! Like the sibling fs tests this target imports `core/tests/common` for its
//! `#[ctor::ctor]`, which runs `serve_sandbox_early_init` so the re-exec child
//! enters Seatbelt instead of landing in the libtest framework, and is gated
//! to macOS, the backend that can confine an in-tree re-exec child without an
//! external helper binary.

#![cfg(all(feature = "test-util", feature = "coreutils", target_os = "macos"))]

mod common;

use ral_core::path::NormalizedPrefix;
use ral_core::protocol::{Program, Run};
use ral_core::types::{Capabilities, FsPolicy, Shell, Value};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};

fn boot() -> Shell {
    ral_core::boot::boot_shell(
        ral_core::io::TerminalState::default(),
        common::prelude(),
        &ral_core::boot::HostSurface::default(),
    )
}

/// A capability frame whose fs policy confines reads and writes to `dir`.  Any
/// `fs` key makes `sandbox_projection()` return `Some(_)`, so the per-command
/// launcher confines the `printenv` child.
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

/// The environment a confined bundled child sees, as `printenv` reports it.
fn confined_child_env(src: &str) -> String {
    let dir = std::env::temp_dir().join(format!("ral_loader_env_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create work dir");
    let mut shell = boot();
    let report = shell.run(RunRequest {
        run: Run {
            program: Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: restrict_to(&dir.to_string_lossy()),
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
        lifecycle: Box::new(()),
    });
    let RunReport::Ran { ending, .. } = report else {
        panic!("well-formed source must run: {src:?}");
    };
    match ending
        .into_result()
        .expect("a confined bundled printenv must succeed")
    {
        Value::String(s) => s.to_string(),
        other => panic!("expected printenv's bytes as a string, got {other:?}"),
    }
}

#[test]
fn dyld_overrides_never_reach_a_confined_child() {
    let env = confined_child_env(
        // An already-loaded system dylib: inserting it changes nothing, so an
        // unstripped child still reaches `printenv` and the assertion below —
        // rather than aborting in dyld — is what reports the hole.
        "within [env: [DYLD_INSERT_LIBRARIES: '/usr/lib/libSystem.B.dylib', \
         DYLD_LIBRARY_PATH: '/nonexistent', RAL_LOADER_PROBE: 'present']] { \
         printenv | from-string }",
    );
    assert!(
        env.contains("RAL_LOADER_PROBE=present"),
        "control variable missing — the child never received the `within` env, \
         so this test proves nothing about the strip: {env:?}"
    );
    assert!(
        !env.contains("DYLD_"),
        "a dynamic-loader override reached a confined child; on macOS it would \
         load before the trampoline enters Seatbelt: {env:?}"
    );
}
