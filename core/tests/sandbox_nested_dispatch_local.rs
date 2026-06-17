//! Regression: nested-confinement dispatch must run locally when the
//! current process is already inside an OS sandbox.
//!
//! Pre-fix, `run_eval` (core/src/evaluator.rs) chose local vs
//! confined dispatch on `projection.is_none()` alone.  Inside an
//! already-confined child the projection was still `Some(...)` — it is
//! shipped via `Mobile` and re-derived from the inherited
//! capability stack — so every `!{…}` substitution inside a `grant`
//! body re-dispatched to `run_confined`, which spawned a fresh ral
//! grandchild with `--sandbox-projection`.  On macOS that grandchild's
//! second `sandbox_init_with_parameters` call hits Seatbelt's
//! one-shot-per-process limit and returns EPERM.
//!
//! The fix: when the confinement marker is *authenticated* (a genuine
//! sandbox re-exec recorded its capability token), run locally
//! regardless of the projection — the OS gate the user asked for is in
//! effect from the outer grant and `crate::capability::check` still
//! fires per operation.  Authentication, not the bare presence of
//! `RAL_SANDBOX_ACTIVE`, is what proves "genuinely already confined"
//! (review finding S1 closed the unauthenticated downgrade); this test
//! simulates the genuine state via the `adopt_token_for_test` seam, the
//! same record the IPC child performs on an authenticated request.
//!
//! The `adopt_token_for_test` seam is gated behind the `test-util`
//! feature, so this whole target compiles only when that feature is on.
//!
//! This file is its own integration-test target on purpose.  It
//! **deliberately does not import** `core/tests/common`, whose
//! `#[ctor::ctor]` calls `ral_core::sandbox::early_init` and pins
//! `SANDBOX_SELF`.  Without that, `confined_availability()` returns
//! `Unavailable`, so the pre-fix dispatch would route to the
//! fail-closed branch — exactly what this regression test catches if
//! the fix is reverted.
#![cfg(feature = "test-util")]

use ral_core::evaluator;
use ral_core::sandbox::{
    ConfinedAvailability, SANDBOX_ACTIVE_ENV, adopt_token_for_test, confined_availability,
};
use ral_core::types::{Capabilities, FsPolicy, Shell};

/// Single test in this target.  Mutates the process env
/// (`RAL_SANDBOX_ACTIVE`) and restores it on exit — no other tests in
/// this binary can race with that mutation.
#[test]
fn run_eval_dispatches_local_when_genuinely_confined() {
    // Sanity: the file's own assumption.  If a future change pins
    // `SANDBOX_SELF` from elsewhere in this target (a stray `common`
    // import, a ctor), the assertion below would still hold under the
    // fix, but the *meaning* of the test would change: it would no
    // longer prove that the authenticated marker is what made the
    // local path fire (could have been the `Ready` path's downgrade
    // instead).  Fail loudly here rather than silently drifting.
    match confined_availability() {
        ConfinedAvailability::Unavailable(_) => {}
        ConfinedAvailability::Ready => panic!(
            "this test target must run without SANDBOX_SELF pinned; \
             did you add a `common` import or another early_init call?"
        ),
    }

    #[allow(clippy::disallowed_methods)] // marker save/restore, not a basedir
    let prior = std::env::var_os(SANDBOX_ACTIVE_ENV);
    // SAFETY: this is the only test in this target.  Adopting the token
    // records it process-globally *and* stamps it into the env marker,
    // so the marker authenticates — exactly the genuinely-confined-child
    // state.  We restore the env var on drop via the guard below; the
    // recorded token is harmless (no other test runs in this target).
    let _token = adopt_token_for_test();
    let _guard = EnvGuard {
        key: SANDBOX_ACTIVE_ENV,
        prior,
    };

    let mut shell = Shell::default();

    // Active projection so `projection.is_none()` is false.  Any fs
    // policy is enough — the reducer's `saw_fs` branch turns it into a
    // `Restricted(...)` projection.
    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/".into()],
            write_prefixes: vec!["/".into()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };

    // Body is trivial: `return ok`.  We only need to prove the
    // boundary dispatched it locally — if it had routed to the
    // `Unavailable` fail-closed branch, the boundary would have
    // produced "sandbox confinement unavailable" *before* the body ran.
    let comp = std::sync::Arc::new(ral_core::compile("return ok").expect("compile"));

    let result = shell.with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s));

    let value = result.expect(
        "with the confinement marker authenticated, an active projection must route to \
         local dispatch instead of fail-closed; the body should have run successfully",
    );

    // The body returns the identifier `ok` as a variant.  Stringify
    // it; the exact shape isn't load-bearing — that the body
    // *produced* a value (rather than the boundary erroring before
    // it ran) is.
    let printed = format!("{value:?}");
    assert!(
        printed.contains("ok"),
        "expected the body's `return ok` to surface, got: {printed:?}"
    );

    // Status must be 0 — the body succeeded.
    assert_eq!(
        shell.mobile.control.last_status, 0,
        "successful local dispatch must install last_status = 0"
    );
}

struct EnvGuard {
    key: &'static str,
    prior: Option<std::ffi::OsString>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: matches the set_var in the test body; no other tests
        // in this target race on this env var.
        unsafe {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
