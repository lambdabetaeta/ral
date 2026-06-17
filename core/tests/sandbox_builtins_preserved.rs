//! Regression: a confined top-level turn must not clobber the parent
//! shell's builtin table.
//!
//! The wire format cannot carry the builtin table — its entries hold host
//! fn pointers — so `WireContext::into_context` hydrates the returned
//! mobile's table empty.  `sandbox::run_confined` restores the parent's
//! table before `eval_top_level` installs the post-run mobile wholesale;
//! without that, `install_mobile` overwrites the session shell's builtins
//! with the empty wire default after the *first* confined turn, and every
//! later local dispatch loses `cd`/`cat`/the host atoms.
//!
//! Imports `common` so its `#[ctor]` pins `SANDBOX_SELF` (confinement
//! reports `Ready`) and serves the IPC child round-trip in-process.

mod common;

use ral_core::sandbox::{ConfinedAvailability, confined_availability};
use ral_core::types::{Capabilities, FsPolicy, Shell};
use ral_core::{builtins, compile, evaluator};
use std::sync::Arc;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn confined_turn_preserves_parent_builtin_table() {
    // common's ctor pinned SANDBOX_SELF; if confinement is not Ready the
    // turn would fail closed and never exercise the wire return path.
    assert!(
        matches!(confined_availability(), ConfinedAvailability::Ready),
        "common's ctor must pin SANDBOX_SELF so the confined round-trip runs"
    );

    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());

    // The parent's table resolves a core builtin before the turn.
    assert!(
        shell.lookup_builtin("cd").is_some(),
        "cd must be installed before the confined turn"
    );

    // A read/write fs policy yields a Restricted projection, so the turn
    // routes to confined transport — the wire return path under test.
    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/".into()],
            write_prefixes: vec!["/".into()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };
    let comp = Arc::new(compile("return ok").expect("compile"));
    shell
        .with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s))
        .expect("confined turn should succeed");

    // The fix: the parent's builtin table survives the confined turn.
    assert!(
        shell.lookup_builtin("cd").is_some(),
        "confined turn clobbered the parent builtin table — cd no longer dispatches"
    );
}
