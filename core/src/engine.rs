//! The engine side of one session: a booted [`Shell`] and the scopes that stop
//! its runs.
//!
//! Two carriers drive it and differ only in carriage — how a dispatch's events
//! leave, and how a forked session is adopted: [`IdentityTransport`] calls it
//! in this process, the wire carrier frames it over a socket.
//!
//! [`IdentityTransport`]: crate::carrier::IdentityTransport

use std::sync::{Arc, Mutex};

use crate::first_order::FOValue;
use crate::guard::GrantNarrower;
use crate::process::{CancelCause, DurableRoot, ForegroundScope};
use crate::protocol::channel::WriteError;
use crate::protocol::{
    Attach, Control, DispatchId, EngineSpec, Event, Origin, PROTOCOL_VERSION, Report, Root, Run,
};
use crate::sync::LockExt as _;
use crate::types::{DeferredSink, Desk, Fork, Runtime, Shell};

pub(crate) mod probe;

/// One compiled-in boot recipe the engine can be told, at `Attach`, to become.
///
/// Each front-end binary — the REPL, exarch — re-execs itself as its own
/// engine and passes its own table; only the tag crosses the wire, never
/// the function.
pub struct EngineInstaller {
    pub tag: &'static str,
    /// The process half: Σ, builtins, docs. Derived from the binary and the
    /// spec's terminal, once per engine: a root and a child engine each derive
    /// their own, an in-process child shares its parent's.
    pub runtime: fn(&EngineSpec) -> Runtime,
    /// The root-only recipe over a root session: libraries, hints, seeds,
    /// ledgers. An `Err` refuses the attach, in the recipe's own words.
    pub root: fn(&Root, &mut Shell) -> Result<(), String>,
    /// How a seeded or adopted child's grant becomes a ceiling. A field and
    /// not a registered hook: core has no base-tag lexicon to resolve a grant
    /// against, so a host that boots an engine must state the policy its
    /// children are held to.
    pub narrow: GrantNarrower,
}

/// One session's engine: its shell, the runtime every session it adopts
/// shares, the scopes that stop its runs, and the installer it was born from.
pub(crate) struct Engine {
    pub(crate) shell: Shell,
    runtime: Arc<Runtime>,
    scopes: Arc<Scopes>,
    installer: &'static EngineInstaller,
}

/// Where a carrier sends one dispatch's events. An event the fuse refuses
/// comes back `Unencodable`, for its writer to answer.
pub(crate) type Outlet = Arc<dyn Fn(DispatchId, Event) -> Result<(), WriteError> + Send + Sync>;

/// One dispatch's host-facing rails, as its carrier lays them.
pub(crate) struct Rails {
    pub(crate) outlet: Outlet,
    pub(crate) deferred: Option<Arc<dyn DeferredSink>>,
    pub(crate) desk: Desk,
    pub(crate) fork: Fork,
}

/// A dispatch's live surface: every value leaves stamped with its dispatch.
struct Surface {
    id: DispatchId,
    outlet: Outlet,
}

impl crate::types::EventSink for Surface {
    fn emit(&self, ev: &FOValue) {
        if let Err(WriteError::Unencodable(e)) = (self.outlet)(self.id, Event::Surface(ev.clone()))
        {
            crate::errln!("engine: dropped a surface value: {e}");
        }
    }
}

/// The version check, then the installer-table lookup.
fn resolve_installer<'a>(
    installers: &'a [EngineInstaller],
    proto_version: u32,
    installer: &str,
) -> Result<&'a EngineInstaller, String> {
    if proto_version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version mismatch (front-end {proto_version}, engine {PROTOCOL_VERSION})"
        ));
    }
    installers
        .iter()
        .find(|i| i.tag == installer)
        .ok_or_else(|| format!("unknown builtin installer '{installer}'"))
}

impl Engine {
    /// Become what `attach` names: resolve its installer, derive the runtime,
    /// then either run the root recipe (seating the root's cwd and env) or
    /// unseal the child's seal and construct it. No recipe runs in a child.
    ///
    /// # Errors
    /// The attach refusal, in the words of whichever step refused; a seal
    /// that will not unseal refuses the attach.
    pub(crate) fn boot(
        installers: &'static [EngineInstaller],
        attach: &Attach,
    ) -> Result<Self, String> {
        let spec = &attach.engine;
        let installer = resolve_installer(installers, spec.proto_version, &spec.installer)?;
        crate::process::jail::install_guest();
        let runtime = Arc::new((installer.runtime)(spec));
        let shell = match &attach.origin {
            Origin::Root(root) => {
                let mut shell = Shell::root(Arc::clone(&runtime));
                crate::boot::seed_host_env(&mut shell);
                (installer.root)(root, &mut shell)?;
                shell.seed_cwd(root.cwd.clone());
                for (name, value) in &root.env {
                    shell.set_env_var(name, value);
                }
                shell
            }
            Origin::Child(sealed) => {
                let bequest = sealed.unseal()?.decode(&runtime)?;
                Shell::child_of(Arc::clone(&runtime), bequest, installer.narrow)?
            }
        };
        Ok(Self::new(shell, installer))
    }

    /// An engine over `shell`, sharing the runtime it was built on.
    pub(crate) fn new(shell: Shell, installer: &'static EngineInstaller) -> Self {
        let runtime = Arc::clone(&shell.session.runtime);
        let scopes = Arc::new(Scopes {
            dispatches: shell.run_cancel_handle(),
            root: shell.cancel_handle(),
            slot: Mutex::default(),
        });
        Self {
            shell,
            runtime,
            scopes,
            installer,
        }
    }

    pub(crate) fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    pub(crate) fn scopes(&self) -> &Arc<Scopes> {
        &self.scopes
    }

    pub(crate) fn installer(&self) -> &'static EngineInstaller {
        self.installer
    }

    /// Run dispatch `id` under the scope [`Scopes::open`] minted for it.
    pub(crate) fn run(
        &mut self,
        id: DispatchId,
        run: Run,
        rails: Rails,
        scope: &ForegroundScope,
    ) -> Report {
        let req = crate::run::RunRequest {
            surface: Some(Arc::new(Surface {
                id,
                outlet: rails.outlet,
            })),
            deferred: rails.deferred,
            desk: Some(rails.desk),
            fork: Some(rails.fork),
            ..crate::run::RunRequest::from(run)
        };
        self.shell.run_under(scope, req).into_report(&self.shell)
    }
}

/// What stops an engine's runs, shared with whichever thread carries its
/// `Control`: each dispatch's scope hangs off `dispatches`, and everything —
/// workers too — off `root`.
pub(crate) struct Scopes {
    dispatches: ForegroundScope,
    root: DurableRoot,
    slot: Mutex<Slot>,
}

#[derive(Default)]
struct Slot {
    /// The dispatch `Interrupt` and `Cancel` strike. Replaced, never cleared:
    /// a settled run's scope is dead, so striking it is a no-op, and a stale
    /// id names a dispatch the next run does not bear.
    current: Option<(DispatchId, ForegroundScope)>,
    /// A strike that overtook the dispatch it names, with its cause.
    foretold: Option<(DispatchId, CancelCause)>,
}

impl Scopes {
    /// Mint `id`'s scope ahead of its run's frame, so a cancel raised
    /// meanwhile has somewhere to land; struck already if a strike foretold
    /// it.
    pub(crate) fn open(&self, id: DispatchId) -> ForegroundScope {
        let scope = self.dispatches.child();
        let mut slot = self.slot.lock_ignore_poison();
        if let Some((_, cause)) = slot.foretold.take_if(|(pending, _)| *pending == id) {
            scope.cancel(cause);
        }
        slot.current = Some((id, scope.clone()));
        scope
    }

    /// One `Control` verb, meaning the same under either carrier.
    pub(crate) fn apply(&self, control: Control) {
        match control {
            Control::Interrupt(id) => self.aim(id, CancelCause::Interrupted),
            Control::Cancel(id) => self.aim(id, CancelCause::Cancelled),
            Control::Terminate => self.end(CancelCause::Terminated),
            Control::Abort => self.end(CancelCause::Aborted),
        }
    }

    /// Strike dispatch `id` with `cause`, or foretell it until `id` arrives.
    fn aim(&self, id: DispatchId, cause: CancelCause) {
        let mut slot = self.slot.lock_ignore_poison();
        match &slot.current {
            Some((current, scope)) if *current == id => scope.cancel(cause),
            _ => {
                let cause = match slot.foretold {
                    Some((pending, prior)) if pending == id => prior.max(cause),
                    _ => cause,
                };
                slot.foretold = Some((id, cause));
            }
        }
    }

    /// The dispatch `Interrupt` and `Cancel` strike now.
    #[cfg(test)]
    pub(crate) fn current(&self) -> Option<DispatchId> {
        self.slot
            .lock_ignore_poison()
            .current
            .as_ref()
            .map(|(id, _)| *id)
    }

    /// Cancel the dispatch in flight, if there is one.
    pub(crate) fn strike(&self, cause: CancelCause) {
        if let Some((_, scope)) = &self.slot.lock_ignore_poison().current {
            scope.cancel(cause);
        }
    }

    /// Cancel the durable root: every run, and every worker.
    pub(crate) fn end(&self, cause: CancelCause) {
        self.root.cancel(cause);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_installer_matches_known_tag() {
        match resolve_installer(&crate::carrier::testkit::BARE, PROTOCOL_VERSION, "bare") {
            Ok(target) => assert_eq!(target.tag, "bare"),
            Err(msg) => panic!("known tag must resolve, got {msg}"),
        }
    }

    #[test]
    fn resolve_installer_refuses_unknown_tag() {
        match resolve_installer(
            &crate::carrier::testkit::BARE,
            PROTOCOL_VERSION,
            "no-such-installer",
        ) {
            Ok(_) => panic!("unknown tag must be refused"),
            Err(msg) => {
                assert!(msg.contains("unknown builtin installer"));
                assert!(msg.contains("no-such-installer"));
            }
        }
    }

    #[test]
    fn resolve_installer_refuses_protocol_mismatch_before_tag_lookup() {
        match resolve_installer(&crate::carrier::testkit::BARE, PROTOCOL_VERSION + 1, "bare") {
            Ok(_) => panic!("a mismatched protocol version must be refused"),
            Err(msg) => assert!(msg.contains("protocol version mismatch")),
        }
    }

    #[test]
    fn a_sealed_attach_runs_no_root_recipe() {
        static PANICKY: [EngineInstaller; 1] = [crate::carrier::testkit::installer(
            "panicky-root",
            |_| crate::HostSurface::default().runtime(crate::terminal::TerminalState::default()),
            |_, _| panic!("a sealed child runs no root recipe"),
        )];
        let sealed = crate::seed::seal(
            &crate::test_helper::core_shell().bequeath(crate::SpawnGrant::Inherit),
        )
        .expect("a scrubbed bequest seals");
        let mut attach = crate::carrier::testkit::attach(&PANICKY);
        attach.origin = Origin::Child(sealed);
        let engine = Engine::boot(&PANICKY, &attach)
            .unwrap_or_else(|e| panic!("a sealed attach boots: {e}"));
        assert!(engine.shell.session.hooks.is_none());
    }

    #[test]
    fn a_seal_that_will_not_unseal_refuses_the_attach() {
        use crate::first_order::{Bytes, datum::Datum as _};
        let garbage = Bytes::from(&b"not a frame"[..]).encode();
        let mut attach = crate::carrier::testkit::attach(&crate::carrier::testkit::BARE);
        attach.origin =
            Origin::Child(crate::seed::Sealed::decode(&garbage).expect("bytes are a seal"));
        let Err(why) = Engine::boot(&crate::carrier::testkit::BARE, &attach) else {
            panic!("a seal that is not a frame must refuse the attach");
        };
        assert!(why.contains("sealed bequest"), "{why}");
    }
}
