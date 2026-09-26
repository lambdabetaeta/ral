//! The engine seat's seed wire: `hatch` packs a forked shell into an
//! [`EngineSeed`] for a freshly spawned engine process, which applies it once
//! its installer has booted.

use crate::serial::{InternCtx, ScopeTable, SerialEnvSnapshot, WireDecoder};
use crate::spawn_grant::{GrantNarrower, SpawnGrant};
use crate::subprocess::{WireShell, install_wire_shell};
use crate::types::{Settled, Shell};
use serde::{Deserialize, Serialize};

/// A forked shell's scope, wire-ready for `hatch`.
///
/// What it deliberately does not carry — a handle anywhere in the scope or
/// the handler stack, and the hooks (all scrubbed upstream by
/// `Shell::fork_scrubbed`, the one place an identity fork and a wire seed
/// both pass through), terminal authority, the parent's inbox or cancel
/// token, its provider handle — is the parity argument for shipping a seed at
/// all: a fork and a seed must mean the same thing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EngineSeed {
    pub(crate) scope_table: ScopeTable,
    pub shell: WireShell,
    pub captured: SerialEnvSnapshot,
    /// The spawn's grant, still unresolved: frozen against the receiving
    /// engine's own cwd, and meet-narrowed against its ceiling, once hydrated.
    pub(crate) grant: SpawnGrant,
}

/// Reify a forked shell into a wire-ready [`EngineSeed`] — `hatch`'s only
/// producer. `shell` is expected already scrubbed by `Shell::fork_scrubbed` —
/// no handle in its scope or handler stack, no hooks — and this function
/// trusts that law rather than re-checking it: a handle that slips through
/// fails the pack as a fault in ral.
///
/// `hatch` is Linux-only, and tests are its only other callers, so a plain
/// non-Linux, non-test build sees this as unreachable — accurate, not a bug.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn pack_seed(shell: &Shell, grant: SpawnGrant) -> Settled<EngineSeed> {
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

impl EngineSeed {
    /// Hydrate the seed's scope and context into `shell`, then push the
    /// child's own grant layer, resolved against the hydrated shell's cwd —
    /// `narrow`, the booting installer's, answers for the base names core has
    /// no lexicon for. The hydrated stack already carries the parent's layers,
    /// so this only adds the child's.
    ///
    /// # Errors
    /// Returns a sentence naming a decode failure, a restriction record the
    /// capability decoder will not read, or whatever `narrow` refuses a base
    /// with — a seeded child is refused rather than admitted above its ceiling.
    pub(crate) fn apply(self, shell: &mut Shell, narrow: GrantNarrower) -> Result<(), String> {
        let dec = WireDecoder::for_shell(shell, &self.scope_table).map_err(|e| {
            format!(
                "hatch: the seed's scope table failed to decode: {}",
                e.message
            )
        })?;
        install_wire_shell(self.shell, shell, &dec)
            .map_err(|e| format!("hatch: the seed's context failed to decode: {}", e.message))?;
        shell.env = self.captured.into_runtime(&dec).map_err(|e| {
            format!(
                "hatch: the seed's captured environment failed to decode: {}",
                e.message
            )
        })?;
        self.grant.narrow_onto(shell, narrow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::BakedPrelude;
    use crate::source::{FileId, Span};
    use crate::subprocess::bare_child_shell;
    use crate::types::{
        DefaultPolicy, Fork, HookName, HookSig, Mooring, Nursery, Value, block_over, captured,
        idle_handle,
    };
    use std::sync::{Arc, OnceLock};

    fn prelude() -> &'static BakedPrelude {
        static P: OnceLock<BakedPrelude> = OnceLock::new();
        P.get_or_init(BakedPrelude::bake_runtime)
    }

    /// The one snapshot law: an identity fork and a wire-seeded child, both
    /// read out of the same nursery slot, resolve every name to the same
    /// value — and the same absence — because `fork_into_nursery` scrubs
    /// every handle, wherever it stands, before either arm sees the shell.
    #[test]
    fn identity_fork_and_wire_seed_agree_on_the_scrubbed_scope() {
        let mut parent = bare_child_shell(prelude());
        parent.set_var("kept".to_string(), Value::Int(7));
        parent.set_var("live".to_string(), idle_handle());
        let blk = block_over(&parent.env);
        parent.set_var("blk".to_string(), blk.clone());
        let has = parent
            .session
            .builtins
            .value("has")
            .expect("`has` is a builtin");
        parent.set_var(
            "nat".to_string(),
            Value::Native {
                entry: Arc::new(has),
                applied: Box::new([idle_handle()]),
            },
        );
        let catch_all = block_over(&parent.env);
        parent.context.handlers.push(Vec::new(), Some(catch_all));
        parent
            .register_hook(
                HookName::session("prompt"),
                blk,
                HookSig::Prompt,
                DefaultPolicy::denied(),
                Span {
                    start: 0,
                    end: 0,
                    file: FileId::DUMMY,
                },
            )
            .expect("register a hook");

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
        // `EngineSeed::apply` does.
        let id_b = parent
            .fork_into_nursery(&mooring)
            .expect("a nursery is installed");
        let nursery_shell = nursery.adopt(id_b).expect("adopt the parked fork");
        let seed =
            pack_seed(&nursery_shell, SpawnGrant::Base("confined".to_string())).expect("pack seed");
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
        let opaque = |v: Option<&Value>| matches!(v, Some(Value::Variant { label, .. }) if label.as_ref() == crate::serial::OPAQUE_TAG);
        for (arm, child) in [
            ("an identity fork", &identity_child),
            ("a wire seed", &wire_child),
        ] {
            assert!(
                opaque(child.env.get("live")),
                "{arm} must scrub a handle-carrying binding"
            );
            assert!(
                opaque(captured(child.env.get("blk")).get("live")),
                "{arm} must scrub the handle a block's captured scope binds"
            );
            let frame = child
                .context
                .handlers
                .iter()
                .find_map(|f| f.catch_all.as_ref());
            assert!(
                opaque(captured(frame).get("live")),
                "{arm} must scrub the handle a handler frame's scope binds"
            );
            let Some(Value::Native { applied, .. }) = child.env.get("nat") else {
                panic!("{arm} must keep a partially applied native");
            };
            assert!(
                opaque(applied.first()),
                "{arm} must scrub a native's applied handle"
            );
            assert!(
                child.context.hooks.is_empty(),
                "{arm} must leave the parent's hooks behind"
            );
        }
    }

    /// Rows are per closure: `big` rides the session's row and `h`'s, and
    /// `f` and `g`, mentioning nothing, share the one empty row.
    #[test]
    fn a_seed_carries_a_binding_once_per_closure_that_mentions_it() {
        const MARKER: &str = "zqx-marker";
        let mut shell = bare_child_shell(prelude());
        crate::evaluator::run_source(
            &format!("let big = '{MARKER}'\nlet f = {{ 1 }}\nlet g = {{ 2 }}\nlet h = {{ $big }}"),
            &mut shell,
        )
        .expect("define");
        let seed = pack_seed(
            &shell.fork_scrubbed(),
            SpawnGrant::Base("confined".to_string()),
        )
        .expect("pack seed");
        let marked = seed
            .scope_table
            .iter()
            .filter(|row| serde_json::to_string(row).expect("encode").contains(MARKER))
            .count();
        assert_eq!(marked, 2, "`big` is written once per row that holds it");
        assert_eq!(
            seed.scope_table.iter().filter(|row| row.is_empty()).count(),
            1,
            "every empty capture interns to one row"
        );
    }
}
