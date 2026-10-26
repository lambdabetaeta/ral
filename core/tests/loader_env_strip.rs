#![allow(clippy::disallowed_methods, reason = "test code")]

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
//! `#[ctor::ctor]`, which runs `invocation::serve_process_before_main` so the re-exec child
//! enters Seatbelt instead of landing in the libtest framework, and is gated
//! to macOS, the backend that can confine an in-tree re-exec child without an
//! external helper binary.

#![cfg(all(feature = "test-util", feature = "coreutils", target_os = "macos"))]

mod common;

use ral_core::RunIo;
use ral_core::authority::policy::{Capabilities, EditorVerb, FsPolicy, ShellVerb};
use ral_core::path::FrozenPath;
use ral_core::protocol::Run;
use ral_core::shell::run::RunReport;
use ral_core::shell::runtime::{Shell, Value};
use ral_core::source::SourceOrigin;
use ral_core::test_helper::{layer, stack};
use std::collections::BTreeSet;
use strum::VariantArray;

fn boot() -> Shell {
    ral_core::shell::boot::boot_shell(
        ral_core::io::terminal::TerminalState::default(),
        common::prelude(),
        &ral_core::shell::boot::HostSurface::default(),
        ral_core::test_helper::process_cwd(),
    )
    .expect("a test host resolves its baseline")
}

/// A capability frame whose fs policy confines reads and writes to a scratch
/// directory.  Any `fs` key makes `sandbox_projection()` return `Some(_)`, so the per-command
/// launcher confines the `printenv` child.
fn restrict() -> Capabilities {
    let dir = std::env::temp_dir().join(format!("ral_loader_env_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create work dir");
    let prefix = FrozenPath::freeze(&dir).expect("a Unicode scratch path");
    Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![prefix.clone()],
            write_prefixes: vec![prefix],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::default()
    }
}

/// Layers that supply an axis only to list every verb, or none at all.
fn neutral_layers() -> [Capabilities; 4] {
    let editor: BTreeSet<_> = EditorVerb::VARIANTS.iter().copied().collect();
    let shell: BTreeSet<_> = ShellVerb::VARIANTS.iter().copied().collect();
    [
        Capabilities::default(),
        Capabilities {
            editor: Some(editor.clone()),
            ..Capabilities::default()
        },
        Capabilities {
            shell: Some(shell.clone()),
            ..Capabilities::default()
        },
        Capabilities {
            editor: Some(editor),
            shell: Some(shell),
            ..Capabilities::default()
        },
    ]
}

/// The environment a bundled child sees under `layers`, as `printenv` reports
/// it, with `vars` set in a `within`.
fn child_env(layers: impl IntoIterator<Item = Capabilities>, vars: &str) -> String {
    let caps = stack(layers.into_iter().map(layer));
    let src = format!("within [env: [{vars}]] {{ printenv | from-string }}");
    let report = boot().run(Run {
        io: RunIo::Inherit,
        caps,
        ..Run::captured(src.as_str(), SourceOrigin::named("<test>"))
    });
    let RunReport::Ran { ending, .. } = report else {
        panic!("well-formed source must run: {src:?}");
    };
    match ending
        .into_result()
        .expect("a bundled printenv must succeed")
    {
        Value::String(s) => s.into_string(),
        other => panic!("expected printenv's bytes as a string, got {other:?}"),
    }
}

// An already-loaded system dylib: inserting it changes nothing, so an
// unstripped child still reaches `printenv` and the assertion below, rather
// than aborting in dyld, is what reports the hole.
const LOADER_VARS: &str = "DYLD_INSERT_LIBRARIES: '/usr/lib/libSystem.B.dylib', \
                           DYLD_LIBRARY_PATH: '/nonexistent', RAL_LOADER_PROBE: 'present'";

fn assert_stripped(env: &str) {
    assert!(
        env.contains("RAL_LOADER_PROBE=present"),
        "control variable missing: the child never received the `within` env, \
         so this test proves nothing about the strip: {env:?}"
    );
    assert!(
        !env.contains("DYLD_"),
        "a dynamic-loader override reached a confined child; on macOS it would \
         load before the trampoline enters Seatbelt: {env:?}"
    );
}

#[test]
fn dyld_overrides_never_reach_a_confined_child() {
    assert_stripped(&child_env([restrict()], LOADER_VARS));
}

/// A withheld verb alone activates the strip.
#[test]
fn a_withheld_verb_strips_loader_overrides() {
    let withheld = Capabilities {
        shell: Some(BTreeSet::from([ShellVerb::Chdir])),
        ..Capabilities::default()
    };
    assert_stripped(&child_env([withheld], LOADER_VARS));
}

/// Neutral layers around a restriction cannot turn the strip off.
#[test]
fn neutral_layers_around_a_restriction_keep_stripping() {
    for neutral in neutral_layers() {
        let layers = [neutral.clone(), restrict(), neutral];
        assert_stripped(&child_env(layers, LOADER_VARS));
    }
}

/// A neutral policy launches ambient: the override arrives.
#[test]
fn a_neutral_policy_leaves_loader_overrides_alone() {
    for neutral in neutral_layers() {
        let env = child_env([neutral], LOADER_VARS);
        assert!(
            env.contains("DYLD_LIBRARY_PATH=/nonexistent"),
            "a neutral policy must not scrub the environment: {env:?}"
        );
    }
}
