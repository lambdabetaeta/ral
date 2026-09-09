//! The engine seat's seed wire: `hatch` packs a forked shell into an
//! [`EngineSeed`] for a freshly spawned engine process.

use crate::serial::{InternCtx, ScopeTable, SerialEnvSnapshot};
use crate::subprocess::WireShell;
use crate::types::{Settled, Shell};
use serde::{Deserialize, Serialize};

/// A forked shell's scope, wire-ready for `hatch`.
///
/// What it deliberately does not carry — `Value::Handle` bindings (scrubbed
/// upstream, at `Shell::fork_scrubbed`, the one place both an identity
/// fork and a wire seed pass through), terminal authority, the parent's
/// inbox or cancel token, its provider handle — is the parity argument for
/// shipping a seed at all: a fork and a seed must mean the same thing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EngineSeed {
    pub(crate) scope_table: ScopeTable,
    pub shell: WireShell,
    pub captured: SerialEnvSnapshot,
    /// The spawn's validated base tag, meet-narrowed against the receiving
    /// engine's own ceiling once hydrated.
    pub(crate) grant: String,
}

/// Reify a forked shell into a wire-ready [`EngineSeed`] — `hatch`'s only
/// producer. `shell` is expected already scrubbed by `Shell::fork_scrubbed`;
/// this function trusts that law rather than re-checking it.
///
/// `hatch` is Linux-only, and `crate::hatch`'s own tests are its only other
/// caller, so a plain non-Linux, non-test build sees this as unreachable —
/// accurate, not a bug.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn pack_seed(shell: &Shell, grant: String) -> Settled<EngineSeed> {
    let mut ctx = InternCtx::new();
    let captured = SerialEnvSnapshot::from_runtime(&shell.env, &mut ctx);
    let wire_shell = WireShell::from_runtime(
        &shell.env,
        shell.session.stack_limit,
        &shell.context,
        &mut ctx,
    )?;
    Ok(EngineSeed {
        scope_table: ctx.finish()?,
        shell: wire_shell,
        captured,
        grant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::BakedPrelude;
    use crate::serial::WireDecoder;
    use crate::subprocess::{bare_child_shell, install_wire_shell};
    use crate::types::{Fork, Mooring, Nursery, Value};
    use std::sync::{Arc, OnceLock};

    fn prelude() -> &'static BakedPrelude {
        static P: OnceLock<BakedPrelude> = OnceLock::new();
        P.get_or_init(BakedPrelude::bake_runtime)
    }

    /// The one snapshot law: an identity fork and a wire-seeded child, both
    /// read out of the same nursery slot, resolve every name to the same
    /// value — and the same absence — because `fork_into_nursery` scrubs
    /// `Value::Handle` bindings before either arm ever sees the scope.
    #[test]
    fn identity_fork_and_wire_seed_agree_on_the_scrubbed_scope() {
        use std::sync::Mutex;

        let mut parent = Shell::default();
        parent.set_var("kept".to_string(), Value::Int(7));
        parent.set_var(
            "live".to_string(),
            Value::Handle(Box::new(crate::types::HandleInner {
                result: Arc::new(Mutex::new(None)),
                cached: Arc::new(Mutex::new(None)),
                state: Arc::new(Mutex::new(crate::types::HandleState::Running)),
                stdout_buf: crate::io::ByteBuffer::default(),
                stderr_buf: crate::io::ByteBuffer::default(),
                surface_buf: Arc::new(Mutex::new(Vec::new())),
                joined: Arc::new(Mutex::new(false)),
                last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
                cmd: "<test>".into(),
                cancel: crate::process::CancelScope::default(),
            })),
        );

        let nursery = Nursery::default();
        let mooring = Mooring {
            fork: Some(Fork::Park(nursery.clone())),
            ..Mooring::adrift()
        };

        // Arm A: identity — adopt straight out of the nursery.
        let id_a = parent
            .fork_into_nursery(&mooring)
            .expect("a nursery is installed");
        let identity_child = nursery.adopt(id_a).expect("adopt the parked fork");

        // Arm B: wire — pack the (separately parked, equally scrubbed) fork
        // into an `EngineSeed` and hydrate a fresh shell from it, exactly as
        // `hatch::apply_seed` does.
        let id_b = parent
            .fork_into_nursery(&mooring)
            .expect("a nursery is installed");
        let nursery_shell = nursery.adopt(id_b).expect("adopt the parked fork");
        let seed = pack_seed(&nursery_shell, "confined".to_string()).expect("pack seed");
        let mut wire_child = bare_child_shell(prelude());
        let dec = WireDecoder::for_shell(&wire_child, &seed.scope_table).expect("decoder");
        install_wire_shell(seed.shell, &mut wire_child, &dec).expect("install shell");
        wire_child.env = seed.captured.into_runtime(&dec).expect("decode captured");

        assert_eq!(
            identity_child.env.get("kept"),
            wire_child.env.get("kept"),
            "both arms must resolve a kept binding to the same value"
        );
        assert_eq!(
            identity_child.env.get("absent"),
            wire_child.env.get("absent"),
            "both arms must agree on the same absence"
        );
        let opaque = |v: Option<&Value>| matches!(v, Some(Value::Variant { label, .. }) if label == crate::serial::OPAQUE_TAG);
        assert!(
            opaque(identity_child.env.get("live")),
            "an identity fork must scrub a handle-carrying binding"
        );
        assert!(
            opaque(wire_child.env.get("live")),
            "a wire seed must scrub the same binding the same way"
        );
    }
}
