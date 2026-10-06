//! The engine seat's seed: a forked shell, packed for a freshly spawned engine.
//!
//! `hatch` ships an [`EngineSeed`] to the process it spawns, which applies it
//! once its installer has booted. [`table`] transports the values and closures
//! inside; this module is the envelope around them.
//!
//! Nothing host-local rides: the builtin table holds fn pointers, hooks are
//! host lifecycle entry points, and IO and session state belong to whoever
//! runs, the child constructing its own. The scrub that makes this safe is
//! `Shell::fork_scrubbed`: no handle in the scope or the handler stack, and no
//! terminal authority, inbox, cancel token or provider handle, is the parity
//! argument for shipping a seed at all: a fork and a seed must mean the same
//! thing.

use crate::guard::{GrantNarrower, SpawnGrant};
use crate::types::{Context, Error, FrameKind, HandlerEntry, HandlerFrame, Settled, Shell};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use table::{InternCtx, ScopeTable, SerialEnvSnapshot, SerialValue, WireDecoder};

#[cfg(unix)]
pub mod hatch;
mod table;

/// Wire mirror of a user-installed [`HandlerFrame`].
///
/// Hydration does not re-check arity: a per-name entry is unary by
/// construction ([`HandlerEntry::ral_per_name`]) and the sender vetted the
/// thunk at install.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireHandlerFrame {
    entries: Vec<(String, SerialValue, Option<crate::ty::Scheme>)>,
    catch_all: Option<SerialValue>,
    kind: FrameKind,
}

impl WireHandlerFrame {
    fn from_runtime(frame: &HandlerFrame, ctx: &mut InternCtx) -> Result<Self, Error> {
        let entries = frame
            .entries
            .iter()
            .map(|entry| {
                Ok((
                    entry.name.as_ref().to_string(),
                    SerialValue::from_runtime(&entry.thunk, ctx)?,
                    entry.scheme.as_deref().cloned(),
                ))
            })
            .collect::<Result<_, Error>>()?;
        Ok(Self {
            entries,
            catch_all: frame
                .catch_all
                .as_ref()
                .map(|value| SerialValue::from_runtime(value, ctx))
                .transpose()?,
            kind: frame.kind,
        })
    }

    fn into_runtime(self, dec: &WireDecoder) -> Result<HandlerFrame, Error> {
        let entries = self
            .entries
            .into_iter()
            .map(|(name, value, scheme)| {
                value.into_runtime(dec).map(|v| {
                    let mut entry = HandlerEntry::ral_per_name(name, v);
                    entry.scheme = scheme.map(Arc::new);
                    entry
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(HandlerFrame {
            entries,
            catch_all: self.catch_all.map(|v| v.into_runtime(dec)).transpose()?,
            kind: self.kind,
        })
    }
}

/// A forked shell, wire-ready for `hatch`: its environment once, its context
/// with the handler frames interned, the cap on its stack, and the spawn's
/// grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EngineSeed {
    scope_table: ScopeTable,
    env: SerialEnvSnapshot,
    /// The cap rides so the child continues the parent's rc / CLI-configured
    /// ceiling rather than the compile-time default.
    stack_limit: usize,
    context: Context<Vec<WireHandlerFrame>>,
    /// The spawn's grant, still unresolved: frozen against the receiving
    /// engine's own cwd, and meet-narrowed against its ceiling, once hydrated.
    grant: SpawnGrant,
}

/// Reify a forked shell into a wire-ready [`EngineSeed`], `hatch`'s only
/// producer. `shell` is expected already scrubbed by `Shell::fork_scrubbed`,
/// and this function trusts that law rather than re-checking it: a handle that
/// slips through fails the pack as a fault in ral.
///
/// `hatch` is Linux-only, and tests are its only other callers, so a plain
/// non-Linux, non-test build sees this as unreachable: accurate, not a bug.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn pack_seed(shell: &Shell, grant: SpawnGrant) -> Settled<EngineSeed> {
    let mut ctx = InternCtx::new();
    let env = SerialEnvSnapshot::from_runtime(&shell.env, &mut ctx);
    let context = shell.context.clone().try_map_handlers(|stack| {
        stack
            .iter()
            .map(|frame| WireHandlerFrame::from_runtime(frame, &mut ctx))
            .collect::<Result<Vec<_>, _>>()
    })?;
    Ok(EngineSeed {
        scope_table: ctx.finish()?,
        env,
        stack_limit: shell.session.stack_limit,
        context,
        grant,
    })
}

/// How a decode failure is worded, naming the part of the seed it was in.
fn failed(part: &'static str) -> impl FnOnce(Error) -> String {
    move |e| format!("hatch: the seed's {part} failed to decode: {}", e.message)
}

impl EngineSeed {
    /// Hydrate the seed's scope and context into `shell`, then push the
    /// child's own grant layer, resolved against the hydrated shell's cwd:
    /// `narrow`, the booting installer's, answers for the base names core has
    /// no lexicon for. The hydrated stack already carries the parent's layers,
    /// so this only adds the child's.
    ///
    /// The receiver's builtin table survives, never having ridden the wire, and
    /// the wire's handler frames splice atop its own. Each goes through
    /// `HandlerStack::push_frame`, which mints a handle from the receiver's
    /// counter, so an alias frame stays removable by `unalias` in the child.
    ///
    /// # Errors
    /// Returns a sentence naming a decode failure, a restriction record the
    /// capability decoder will not read, or whatever `narrow` refuses a base
    /// with: a seeded child is refused rather than admitted above its ceiling.
    pub(crate) fn apply(self, shell: &mut Shell, narrow: GrantNarrower) -> Result<(), String> {
        let dec =
            WireDecoder::for_shell(shell, &self.scope_table).map_err(failed("scope table"))?;
        let context = self
            .context
            .try_map_handlers(|frames| {
                frames
                    .into_iter()
                    .try_fold(shell.context.handlers.clone(), |mut stack, frame| {
                        stack.push_frame(frame.into_runtime(&dec)?);
                        Ok::<_, Error>(stack)
                    })
            })
            .map_err(failed("context"))?;
        let env = self
            .env
            .into_runtime(&dec)
            .map_err(failed("captured environment"))?;
        shell.env = env;
        shell.session.stack_limit = self.stack_limit;
        shell.context = context;
        self.grant.narrow_onto(shell, narrow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::BakedPrelude;
    use crate::types::{
        DefaultPolicy, Fork, HookName, HookSig, Mooring, Nursery, Value, block_over, captured,
        idle_handle,
    };

    /// A shell over core's own surface with the prelude seated: what a
    /// hatched child hydrates into.
    fn child() -> Shell {
        let mut shell = crate::test_helper::core_shell();
        BakedPrelude::runtime().seat(&mut shell);
        shell
    }

    /// A narrower no test here may reach: only a `Base` grant consults one.
    fn no_base(_: &str, _: &std::path::Path) -> Result<crate::capability::Capabilities, String> {
        panic!("only a Base grant consults the narrower")
    }

    /// The one snapshot law: an identity fork and a wire-seeded child, both
    /// read out of the same nursery slot, resolve every name to the same
    /// value — and the same absence — because `fork_into_nursery` scrubs
    /// every handle, wherever it stands, before either arm sees the shell.
    #[test]
    fn identity_fork_and_wire_seed_agree_on_the_scrubbed_scope() {
        let mut parent = child();
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
        let seed = pack_seed(&nursery_shell, SpawnGrant::Inherit).expect("pack seed");
        let mut wire_child = child();
        seed.apply(&mut wire_child, no_base).expect("apply seed");

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
        let opaque = |v: Option<&Value>| matches!(v, Some(Value::Variant { label, .. }) if label.as_ref() == crate::first_order::OPAQUE_TAG);
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
                child.session.hooks.is_empty(),
                "{arm} must leave the parent's hooks behind"
            );
        }
    }

    /// Rows are per closure: `big` rides the session's row and `h`'s, and
    /// `f` and `g`, mentioning nothing, share the one empty row.
    #[test]
    fn a_seed_carries_a_binding_once_per_closure_that_mentions_it() {
        const MARKER: &str = "zqx-marker";
        let mut shell = child();
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
