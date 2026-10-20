//! The engine seat's seed: a [`Bequest`] in wire form.
//!
//! A bequest sealed for an engine the host will make: the sealing engine ships
//! an [`EngineSeed`] as [`Sealed`] bytes the host relays unread, and the child
//! engine decodes it against its own runtime and builds its session with
//! `Shell::child_of`, as an in-process child is built from the parked bequest.
//! [`table`] transports the values and closures inside; this module is the
//! envelope around them.
//!
//! Nothing host-local rides: the builtin table holds fn pointers, hooks are
//! host lifecycle entry points, and IO and session state belong to whoever
//! runs, the child constructing its own. The scrub that makes this safe is
//! `Shell::bequeath`: no handle in the scope or the handler stack, and no
//! terminal authority, inbox, cancel token or provider handle.

use crate::first_order::datum::Datum;
use crate::first_order::{Bytes, FOValue};
use crate::guard::SpawnGrant;
use crate::source::SourceDb;
use crate::ty::Fixings;
use crate::types::{
    Bequest, Context, Error, FrameKind, HandlerEntry, HandlerFrame, Image, Runtime, Terms,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use table::{InternCtx, ScopeTable, SerialEnvSnapshot, SerialValue, WireDecoder};

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

/// A bequest in wire form.  A codec and nothing else.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EngineSeed {
    scope_table: ScopeTable,
    env: SerialEnvSnapshot,
    context: Context<Vec<WireHandlerFrame>>,
    sources: SourceDb,
    terms: Terms,
    fixings: Fixings,
    /// Still unresolved: frozen against the receiving engine's own cwd, and
    /// meet-narrowed against its ceiling, by `Shell::child_of`.
    grant: SpawnGrant,
}

/// A bequest in bytes: what the wire seat's enquiry names and a child's
/// `Attach` carries. The host relays it and never reads it (the opacity
/// law, `decisions/261013_the-seed-rides-the-protocol-sealed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sealed(Bytes);

/// Encode and frame `bequest` into bytes.
///
/// # Errors
/// Returns a sentence naming why the bequest could not be sealed.
pub fn seal(bequest: &Bequest) -> Result<Sealed, String> {
    let sealed = |e: &dyn std::fmt::Display| format!("seed: could not seal the bequest: {e}");
    let seed = EngineSeed::encode(bequest).map_err(|e| sealed(&e))?;
    let frame = crate::frame::encode(&seed).map_err(|e| sealed(&e))?;
    let mut bytes = Vec::new();
    crate::frame::put(&mut bytes, &frame).map_err(|e| sealed(&e))?;
    Ok(Sealed(bytes.into()))
}

impl Sealed {
    /// The frame back out. Refuses, naming the fault, on bytes that are not
    /// one `EngineSeed` frame.
    pub(crate) fn unseal(&self) -> Result<EngineSeed, String> {
        let mut rest: &[u8] = &self.0;
        match crate::frame::read_frame(&mut rest) {
            Err(e) => Err(format!("seed: the sealed bequest is not a seed frame: {e}")),
            Ok(Some(seed)) if rest.is_empty() => Ok(seed),
            Ok(_) => Err("seed: the sealed bequest is not one frame".into()),
        }
    }
}

impl Datum for Sealed {
    fn encode(self) -> FOValue {
        self.0.encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        Bytes::decode(v).map(Self)
    }
}

/// How a decode failure is worded, naming the part of the seed it was in.
fn failed(part: &'static str) -> impl FnOnce(Error) -> String {
    move |e| format!("seed: the seed's {part} failed to decode: {}", e.message)
}

impl EngineSeed {
    /// `bequest`, interned for the wire.  The bequest is expected scrubbed by
    /// `Shell::bequeath`, and this trusts that law rather than re-checking it:
    /// a handle that slips through fails the encode as a fault in ral.
    pub(crate) fn encode(bequest: &Bequest) -> Result<Self, Error> {
        let Image {
            env,
            context,
            sources,
            terms,
            fixings,
        } = &bequest.image;
        let mut ctx = InternCtx::new();
        let env = SerialEnvSnapshot::from_runtime(env, &mut ctx);
        let context = context.clone().try_map_handlers(|frames| {
            frames
                .iter()
                .map(|frame| WireHandlerFrame::from_runtime(frame, &mut ctx))
                .collect::<Result<Vec<_>, _>>()
        })?;
        Ok(Self {
            scope_table: ctx.finish()?,
            env,
            context,
            sources: sources.clone(),
            terms: terms.clone(),
            fixings: fixings.snapshot(),
            grant: bequest.grant.clone(),
        })
    }

    /// Decode against `runtime`, whose builtin table re-links natives by name.
    ///
    /// # Errors
    /// Returns a sentence naming the part of the seed that failed to decode.
    pub(crate) fn decode(self, runtime: &Runtime) -> Result<Bequest, String> {
        let dec =
            WireDecoder::for_runtime(runtime, &self.scope_table).map_err(failed("scope table"))?;
        let context = self
            .context
            .try_map_handlers(|frames| {
                frames
                    .into_iter()
                    .map(|frame| frame.into_runtime(&dec))
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(failed("context"))?;
        let env = self
            .env
            .into_runtime(&dec)
            .map_err(failed("captured environment"))?;
        Ok(Bequest {
            image: Image {
                env,
                context,
                sources: self.sources,
                terms: self.terms,
                fixings: self.fixings,
            },
            grant: self.grant,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::BakedPrelude;
    use crate::capability::Capabilities;
    use crate::sync::LockExt as _;
    use crate::types::{
        DefaultPolicy, Env, HookName, HookSig, Shell, Value, block_over, captured, idle_handle,
    };
    use std::collections::BTreeSet;

    /// The runtime a parent and its children are built over.
    fn runtime() -> crate::Runtime {
        crate::boot::boot_runtime(
            crate::terminal::TerminalState::default(),
            BakedPrelude::runtime(),
            &crate::boot::HostSurface::default(),
        )
    }

    /// The recipe a parent is built by: core's own surface, the prelude
    /// seated, and a session's policy.
    fn booted() -> Shell {
        let mut shell = crate::boot::boot_shell(
            crate::terminal::TerminalState::default(),
            BakedPrelude::runtime(),
            &crate::boot::HostSurface::default(),
            crate::test_helper::process_cwd(),
        );
        shell.set_exit_hints(crate::types::ExitHints::from_text("false 1 hinted\n"));
        shell.arm_binding_lease(crate::types::BindingLease {
            idle_calls: 3,
            large_binding_bytes: 1 << 20,
        });
        shell.arm_worker_retention(5);
        shell.arm_detach(4);
        shell
    }

    /// A narrower no test here may reach: only a Base grant consults one.
    fn no_base(_: &str, _: &std::path::Path) -> Result<Capabilities, String> {
        panic!("only a Base grant consults the narrower")
    }

    /// A booted parent with a binding of every shape the scrub and the codec
    /// must treat: kept, handle-carrying, block, partly applied native, a
    /// frame with a catch-all, a per-name and an alias entry, a hook, an extra source, a fixed unit, and a spent
    /// birth.
    fn parent() -> Shell {
        let mut parent = booted();
        parent.session.stack_limit = 12_345;
        parent
            .fixings()
            .fix(crate::ty::UnitId::fresh(), 0, crate::ty::Side::Number, "/n")
            .expect("a fresh variable is fixed");
        parent.set_var("kept".to_string(), Value::Int(7));
        parent.set_var("live".to_string(), idle_handle());
        let blk = block_over(&parent.env);
        parent.set_var("blk".to_string(), blk.clone());
        let has = parent
            .session
            .runtime
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
        let mut alias = HandlerEntry::ral_per_name("alias".into(), blk.clone());
        alias.scheme = Some(Arc::new(crate::ty::Scheme::mono(crate::ty::Ty::Unit)));
        let named = HandlerEntry::ral_per_name("named".into(), blk.clone());
        parent
            .context
            .handlers
            .push(vec![named, alias], Some(catch_all));
        parent
            .register_hook(
                HookName::session("prompt"),
                blk,
                HookSig::Block,
                DefaultPolicy::denied(),
            )
            .expect("register a hook");
        parent.install_script_context(
            &crate::source::SourceOrigin::File(
                crate::source::SourceFile::new("extra.ral").expect("UTF-8"),
            ),
            "echo extra\n",
        );
        parent
            .detach_policy()
            .expect("armed")
            .admit()
            .expect("a birth is left")
            .commit();
        parent
    }

    /// `bequest` through bytes, and decoded against `runtime`.
    fn through_the_wire(bequest: &Bequest, runtime: &Runtime) -> Bequest {
        let seed = seal(bequest).expect("seal").unseal().expect("unseal");
        seed.decode(runtime).expect("decode")
    }

    fn opaque(v: Option<&Value>) -> bool {
        matches!(v, Some(Value::Variant { label, .. }) if label.as_ref() == crate::first_order::OPAQUE_TAG)
    }

    fn names(env: &Env) -> BTreeSet<String> {
        env.names().map(ToString::to_string).collect()
    }

    /// Values agree across the wire: data by `==`, a closure by its body and
    /// its captured scope, a native by its name and what it was applied to.
    fn agree(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Thunk(x), Value::Thunk(y)) => {
                x.comp() == y.comp() && scopes_agree(x.env(), y.env())
            }
            (
                Value::Native { entry, applied },
                Value::Native {
                    entry: theirs,
                    applied: args,
                },
            ) => {
                entry.decl.name == theirs.decl.name
                    && applied.len() == args.len()
                    && applied.iter().zip(args).all(|(a, b)| agree(a, b))
            }
            _ => a == b,
        }
    }

    fn scopes_agree(a: &Env, b: &Env) -> bool {
        names(a) == names(b)
            && names(a).iter().all(|name| {
                a.get(name)
                    .zip(b.get(name))
                    .is_some_and(|(x, y)| agree(x, y))
            })
    }

    #[test]
    fn a_bequest_survives_the_wire() {
        let parent = parent();
        let b = parent.bequeath(SpawnGrant::Base("confined".into()));
        let d = through_the_wire(&b, &runtime());

        assert_eq!(names(&b.image.env), names(&d.image.env));
        for name in names(&b.image.env) {
            let (ours, theirs) = (b.image.env.get(&name), d.image.env.get(&name));
            assert!(
                ours.zip(theirs).is_some_and(|(a, b)| agree(a, b)),
                "`{name}` must resolve alike on both sides of the wire"
            );
        }

        for (arm, env, frames) in [
            ("the bequest", &b.image.env, &b.image.context.handlers),
            (
                "the decoded bequest",
                &d.image.env,
                &d.image.context.handlers,
            ),
        ] {
            assert!(opaque(env.get("live")), "{arm} must scrub a handle binding");
            assert!(
                opaque(captured(env.get("blk")).get("live")),
                "{arm} must scrub a block's captured handle"
            );
            let catch_all = frames.iter().find_map(|f| f.catch_all.as_ref());
            assert!(
                opaque(captured(catch_all).get("live")),
                "{arm} must scrub a handler frame's captured handle"
            );
            let Some(Value::Native { applied, .. }) = env.get("nat") else {
                panic!("{arm} must keep a partially applied native");
            };
            assert!(
                opaque(applied.first()),
                "{arm} must scrub a native's handle"
            );
        }

        let shape = |frames: &[HandlerFrame]| {
            frames
                .iter()
                .map(|f| {
                    let entries: Vec<_> = f
                        .entries
                        .iter()
                        .map(|e| (e.name.to_string(), e.scheme.is_some()))
                        .collect();
                    (f.kind, entries, f.catch_all.is_some())
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shape(&b.image.context.handlers),
            shape(&d.image.context.handlers)
        );
        assert_eq!(
            shape(&d.image.context.handlers)[0].1,
            [("named".to_string(), false), ("alias".to_string(), true)]
        );
        assert_eq!(b.image.terms, d.image.terms);
        assert_eq!(d.image.terms.stack_limit, 12_345);
        assert_eq!(
            *b.image.fixings.0.lock_ignore_poison(),
            *d.image.fixings.0.lock_ignore_poison()
        );
        assert_eq!(b.image.sources, d.image.sources);
        assert_eq!(b.grant, d.grant);
    }

    /// A child is the same whichever seat built it: from the bequest itself,
    /// or from its decoding.
    #[test]
    fn a_child_is_the_same_from_either_bequest() {
        let parent = parent();
        let b = parent.bequeath(SpawnGrant::Inherit);
        let decoded = through_the_wire(&b, &runtime());
        let layers = parent.context.grants.len();
        let (unit, var) = *parent
            .fixings()
            .0
            .lock_ignore_poison()
            .keys()
            .next()
            .expect("the parent fixed a unit");
        for (arm, bequest) in [("the bequest", b), ("the decoded bequest", decoded)] {
            let child = Shell::child_of(runtime().into(), bequest, no_base).expect("a child");
            assert!(child.session.hooks.is_none(), "{arm}: no hooks");
            assert!(child.session.terminal_lease.is_none(), "{arm}: no lease");
            assert!(matches!(child.io.stdin, crate::io::Source::Empty), "{arm}");
            assert!(
                child.session.exit_hints.lookup("false", 1).is_some(),
                "{arm}: the exit hints"
            );
            assert!(
                child.local.bindings.armed()
                    && child.local.bindings.leased_count() == 0
                    && child.local.bindings.exempts("kept"),
                "{arm}: everything inherited is baseline"
            );
            assert_eq!(child.local.workers.retention(), Some(5), "{arm}");
            let detach = child.detach_policy().expect("armed");
            assert_eq!(detach.budget, 4, "{arm}");
            assert_eq!(detach.born(), 0, "{arm}: the budget is unspent");
            assert_eq!(child.session.stack_limit, 12_345, "{arm}");
            let held = child.fixings().fix(unit, var, crate::ty::Side::Text, "/t");
            assert!(held.is_err(), "{arm}: the parent's fixed unit");
            let fresh = crate::ty::UnitId::fresh();
            child
                .fixings()
                .fix(fresh, 0, crate::ty::Side::Text, "/t")
                .expect("a fresh variable is fixed");
            assert!(
                !parent
                    .fixings()
                    .0
                    .lock_ignore_poison()
                    .contains_key(&(fresh, 0)),
                "{arm}: a child's fixing stays in the child"
            );
            assert_eq!(child.session.sources, parent.session.sources, "{arm}");
            assert_eq!(
                child.context.grants.len(),
                layers + 1,
                "{arm}: the grant layer"
            );
        }
    }

    /// Rows are per closure: `big` rides the session's row and `h`'s, and
    /// `f` and `g`, mentioning nothing, share the one empty row.
    #[test]
    fn a_bequest_carries_a_binding_once_per_closure_that_mentions_it() {
        const MARKER: &str = "zqx-marker";
        let mut shell = booted();
        crate::evaluator::run_source(
            &format!("let big = '{MARKER}'\nlet f = {{ 1 }}\nlet g = {{ 2 }}\nlet h = {{ $big }}"),
            &mut shell,
        )
        .expect("define");
        let seed = EngineSeed::encode(&shell.bequeath(SpawnGrant::Base("confined".to_string())))
            .expect("encode");
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

    #[test]
    fn a_seal_is_first_order_bytes() {
        let sealed = seal(&parent().bequeath(SpawnGrant::Inherit)).expect("seal");
        let wire = sealed.clone().encode();
        assert!(matches!(wire, FOValue::Bytes { .. }));
        assert_eq!(Sealed::decode(&wire).expect("decode"), sealed);
    }

    #[test]
    fn a_seal_that_is_not_one_frame_will_not_unseal() {
        let err = Sealed(Bytes::from(&b"not a frame"[..]))
            .unseal()
            .expect_err("garbage");
        assert!(err.contains("sealed bequest"), "{err}");
    }
}
