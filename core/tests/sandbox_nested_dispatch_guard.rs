//! Regression: the defensive guard in `enter_for_platform`
//! (core/src/sandbox/reexec.rs) must error loudly when reached with
//! `RAL_SANDBOX_ACTIVE` already set.  After the dispatch fix in
//! `core/src/evaluator.rs`, this path is unreachable in practice;
//! the guard is belt-and-braces against a future regression that
//! reintroduces nested respawn.
//!
//! A silent `Ok(None)` here would disable an OS sandbox the user
//! explicitly asked for via `--sandbox-projection`.  The guard's
//! contract is that it surfaces a clear error instead.
//!
//! Driven by `ral_core::sandbox::early_init` directly so the test
//! stays in-process — no separate ral binary needed.  Single test in
//! its own target so the `RAL_SANDBOX_ACTIVE` set/restore can't race
//! other tests.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use ral_core::sandbox::SANDBOX_ACTIVE_ENV;

#[test]
fn early_init_refuses_nested_os_sandbox_when_active_env_set() {
    #[allow(clippy::disallowed_methods)] // marker save/restore, not a basedir
    let prior = std::env::var_os(SANDBOX_ACTIVE_ENV);
    // SAFETY: this is the only test in this target; no parallel
    // tests read or write this env var.  Restored on drop.
    unsafe { std::env::set_var(SANDBOX_ACTIVE_ENV, "1") };
    let _guard = EnvGuard {
        key: SANDBOX_ACTIVE_ENV,
        prior,
    };

    // A well-formed projection — exact shape isn't load-bearing,
    // the guard fires before the policy is interpreted.  Use the
    // wire format that `early_init`'s `--sandbox-projection`
    // consumes (JSON-encoded `SandboxProjection`).  Build it via
    // serde so the test doesn't bind to a hand-spelled field
    // layout that could rot.
    let policy = ral_core::types::SandboxProjection::default();
    let policy_json = serde_json::to_string(&policy).expect("serialise projection");

    let argv: Vec<String> = vec![
        "--sandbox-projection".to_string(),
        policy_json,
        "-c".to_string(),
        "echo ok".to_string(),
    ];

    let err = match ral_core::sandbox::early_init(&argv) {
        Err(e) => e,
        Ok(_) => panic!(
            "early_init must refuse to apply nested OS sandbox when \
             {}=1, but it returned Ok",
            SANDBOX_ACTIVE_ENV
        ),
    };

    assert!(
        err.contains("refusing to apply nested OS sandbox"),
        "guard message must clearly state the refusal; got: {err:?}"
    );
    assert!(
        err.contains(SANDBOX_ACTIVE_ENV),
        "guard message must name the env var so the diagnosis is obvious; \
         got: {err:?}"
    );
}

struct EnvGuard {
    key: &'static str,
    prior: Option<std::ffi::OsString>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see set_var above.
        unsafe {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
