//! Adversarial (review finding S8): a bare or forged `RAL_SANDBOX_ACTIVE`
//! must NOT suppress OS confinement.
//!
//! The OS sandbox is the sole enforcer of `net` and of bundled-coreutils
//! filesystem access ([[design/two-enforcers]]).  Before the
//! authenticated-marker fix, `transport::dispatch` ran every restrictive
//! `grant` body in-process the moment `RAL_SANDBOX_ACTIVE` was merely
//! *present* — a public-name env var any ancestor process can export.  An
//! attacker who set it once would silently downgrade `grant [net: false]`
//! and `grant [fs: …]` to unconfined local evaluation.
//!
//! The fix authenticates the marker against a per-re-exec random token
//! that only ral's own sandbox re-exec mints and delivers over the
//! already-authenticated IPC request channel (`ChildEvalRequest`).  A
//! marker value that does not match a token *recorded from a wire
//! request* is not trusted, so confinement is still attempted.
//!
//! This target deliberately does **not** import `core/tests/common`,
//! whose `#[ctor::ctor]` pins `SANDBOX_SELF`.  Without that pin,
//! `confined_availability()` is `Unavailable`, so an *attempted*
//! confinement surfaces as the fail-closed "sandbox confinement
//! unavailable" error — the observable proof that dispatch refused to run
//! the body locally under the forged marker.  Pre-fix the body would have
//! run locally and succeeded; that divergence is the regression this
//! pins.  Single test in its own target so the `RAL_SANDBOX_ACTIVE`
//! mutation cannot race other tests.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use ral_core::evaluator;
use ral_core::sandbox::{ConfinedAvailability, SANDBOX_ACTIVE_ENV, confined_availability};
use ral_core::types::{Capabilities, FsPolicy, Shell};

#[test]
fn forged_sandbox_marker_does_not_suppress_confinement() {
    // File assumption: no `SANDBOX_SELF` pin in this target, so an
    // attempted confinement routes to the fail-closed branch.  If a
    // future change pins it (a stray `common` import, a ctor), the
    // observable below would change meaning — fail loudly here.
    match confined_availability() {
        ConfinedAvailability::Unavailable(_) => {}
        ConfinedAvailability::Ready => panic!(
            "this test target must run without SANDBOX_SELF pinned; \
             did you add a `common` import or another early_init call?"
        ),
    }

    #[allow(clippy::disallowed_methods)] // marker save/restore, not a basedir
    let prior = std::env::var_os(SANDBOX_ACTIVE_ENV);
    // SAFETY: this is the only test in this target; restored on drop.
    // A *forged* value: no genuine sandbox re-exec recorded a token, so
    // the marker is unauthenticated.
    unsafe { std::env::set_var(SANDBOX_ACTIVE_ENV, "forged-by-attacker") };
    let _guard = EnvGuard {
        key: SANDBOX_ACTIVE_ENV,
        prior,
    };

    assert_net_restriction_enforced();
    assert_fs_restriction_enforced();
}

/// `grant [net: false] { … }` under the forged marker must refuse to run
/// the body locally — net has no in-process gate, so local dispatch is a
/// full-network bypass.
fn assert_net_restriction_enforced() {
    let mut shell = Shell::default();
    let caps = Capabilities {
        net: Some(false),
        ..Capabilities::root()
    };
    let comp = std::sync::Arc::new(ral_core::compile("return ok").expect("compile"));
    let result = shell.with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s));
    assert_confinement_was_attempted(result, "net: false");
}

/// An fs-restricting grant under the forged marker must likewise refuse
/// local dispatch — bundled coreutils touch the filesystem with no
/// in-process `check_fs_*`, so the OS layer is the only fs enforcer.
fn assert_fs_restriction_enforced() {
    let mut shell = Shell::default();
    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/tmp/ral-forged-marker-safe".into()],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };
    let comp = std::sync::Arc::new(ral_core::compile("return ok").expect("compile"));
    let result = shell.with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s));
    assert_confinement_was_attempted(result, "fs restriction");
}

/// The boundary must have routed to confined dispatch and hit the
/// fail-closed branch (no `SANDBOX_SELF` in this target) rather than
/// running the body locally.
fn assert_confinement_was_attempted(
    result: ral_core::types::Settled<ral_core::types::Value>,
    label: &str,
) {
    match result {
        Ok(value) => panic!(
            "forged RAL_SANDBOX_ACTIVE suppressed confinement for {label}: \
             the body ran locally and produced {value:?} instead of \
             attempting confinement"
        ),
        Err(ral_core::types::Break::Error(e)) => assert!(
            e.message.contains("sandbox confinement unavailable"),
            "expected fail-closed confinement error for {label}, got: {}",
            e.message
        ),
        Err(other) => panic!("expected fail-closed error for {label}, got: {other:?}"),
    }
}

struct EnvGuard {
    key: &'static str,
    prior: Option<std::ffi::OsString>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: matches the set_var above; no other tests in this
        // target race on this env var.
        unsafe {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
