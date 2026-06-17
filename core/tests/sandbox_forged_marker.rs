//! Adversarial (review finding S8): a bare or forged `RAL_SANDBOX_ACTIVE`
//! must NOT suppress OS confinement.
//!
//! `RAL_SANDBOX_ACTIVE` is a public-name env var any ancestor process can
//! export. The per-command sandbox launcher in
//! `runtime::command::process::build_command` gates on
//! `!marker_authenticated() && sandbox_projection().is_some()`:
//! confinement fires unless this process is *genuinely* confined, which is
//! proven by the marker's value matching a per-re-exec capability token
//! (`marker::authenticated`), not by the marker's mere presence. A forged
//! value has no matching recorded token, so `marker_authenticated()`
//! returns false, `!marker_authenticated()` stays true, and the per-command
//! sandbox still confines every external child.
//!
//! The regression this pins: if a forged marker *did* authenticate (the
//! pre-S8 "presence is enough" bug), the gate would flip to the
//! run-unconfined branch and an external command writing outside an fs
//! grant would escape — the dangerous write would land. So the
//! load-bearing observation is that, under a forged marker, the
//! out-of-grant write still does **not** land while the same command with
//! no projection at all *does* write (proving the command works and the
//! path is writable, so the denial is the projection, not a broken `sh`).
//!
//! Net has no in-process gate, so for `net` the OS sandbox is the sole
//! enforcer; a `net: false` grant produces a non-`None`
//! `sandbox_projection()` (`saw_net && !net_allowed`) exactly as an fs
//! grant does, so the same per-command gate covers it — the
//! forged-marker-does-not-suppress property holds for net by the identical
//! `!marker_authenticated()` term. Proving net is actually blocked needs a
//! network-attempting external and a backend (macOS omits `(allow
//! network*)`; the unenforceable-backend fail-closed is the unit test
//! `sandbox::tests::projection_enforceable_rejects_net_false_on_windows`);
//! that is not re-driven here. The fs discriminator below is the honest,
//! in-scope proof that a forged marker does not flip the gate.
//!
//! This target **imports** `core/tests/common`: its `#[ctor::ctor]` runs
//! `serve_sandbox_early_init`, which is what lets the per-command re-exec
//! child enter Seatbelt and exec the target inside it. It is gated to
//! macOS, matching the end-to-end denial tests in `sandbox/launch.rs` and
//! `sandbox_fail_closed.rs`. Single test in its own target so the
//! `RAL_SANDBOX_ACTIVE` mutation cannot race other tests.

#![cfg(all(feature = "test-util", target_os = "macos"))]

mod common;

use ral_core::evaluator;
use ral_core::sandbox::SANDBOX_ACTIVE_ENV;
use ral_core::types::{Break, Capabilities, FsPolicy, Shell, Value};

fn boot() -> Shell {
    ral_core::host::boot_shell(Default::default(), common::prelude())
}

fn run(shell: &mut Shell, caps: Capabilities, src: &str) -> ral_core::types::Settled<Value> {
    let comp = match ral_core::compile_and_typecheck(src, shell.session_schemes()) {
        ral_core::CompileOutcome::Compiled(c) => std::sync::Arc::new(c),
        ral_core::CompileOutcome::Parse(e) => panic!("parse: {e}"),
        ral_core::CompileOutcome::Types(errs) => {
            let msgs: Vec<_> = errs.iter().map(|e| e.kind.render_message()).collect();
            panic!("type: {}", msgs.join("; "));
        }
    };
    shell.with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s))
}

#[test]
fn forged_sandbox_marker_does_not_suppress_confinement() {
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

    let work = std::env::temp_dir().join(format!("ral_forged_{}", std::process::id()));
    std::fs::create_dir_all(&work).expect("work dir");
    let work_s = work.to_string_lossy().into_owned();

    // The single path both probes target: outside the grant's write
    // prefix, so the projection must deny it.
    let target = std::env::temp_dir().join(format!("ral_forged_target_{}", std::process::id()));
    let _ = std::fs::remove_file(&target);
    let target_s = target.to_string_lossy().into_owned();

    // Discriminating control: forged marker + NO projection. With no fs/net
    // grant, `sandbox_projection()` is `None`, the per-command gate does
    // not fire, and the external writes normally. This proves the forged
    // marker alone does not break ordinary exec and that `target` is a
    // writable path — so the denial below is the projection enforcing,
    // not a broken command or an unwritable destination.
    let unconfined = run(
        &mut boot(),
        Capabilities::root(),
        &format!("sh -c 'echo x > {target_s}'"),
    );
    unconfined.expect("forged marker + no projection: the external must run normally");
    assert!(
        target.exists(),
        "control write (no projection) should have landed at {target_s}"
    );
    let _ = std::fs::remove_file(&target);

    // The property: forged marker + restrictive fs grant. If the forged
    // marker authenticated (the S8 regression), the gate would skip
    // confinement and this out-of-grant write would land. It must not.
    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![work_s.clone().into()],
            write_prefixes: vec![work_s.clone().into()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };
    let confined = run(&mut boot(), caps, &format!("sh -c 'echo x > {target_s}'"));
    match confined {
        Err(Break::Error(_)) => {}
        Err(other) => panic!("expected the confined external to fail, got {other:?}"),
        Ok(v) => panic!(
            "forged RAL_SANDBOX_ACTIVE suppressed confinement: the external \
             ran and produced Ok({v:?}) instead of being confined"
        ),
    }
    assert!(
        !target.exists(),
        "forged marker must not suppress confinement: the out-of-grant write \
         must not have landed at {target_s}"
    );

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&target);
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
