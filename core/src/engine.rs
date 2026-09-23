//! The engine side of one session: a booted [`Shell`] and the scopes that stop
//! its runs.
//!
//! Two carriers drive it and differ only in carriage — how a dispatch's events
//! leave, and how a forked session is adopted: [`IdentityTransport`] calls it
//! in this process, [`wire`] frames it over a socket.
//!
//! [`IdentityTransport`]: crate::protocol::IdentityTransport

use std::sync::{Arc, Mutex};

use crate::engine_seed::EngineSeed;
use crate::process::{CancelCause, DurableRoot, ForegroundScope};
use crate::protocol::{Attach, Control, DispatchId, Event, PROTOCOL_VERSION, Report, Run};
use crate::serial::FOValue;
use crate::spawn_grant::GrantNarrower;
use crate::sync::LockExt as _;
use crate::types::{DeferredSink, Desk, Fork, Shell};

#[cfg(unix)]
mod wire;
#[cfg(unix)]
pub(crate) use wire::HOST_SILENCE_DEADLINE;
#[cfg(unix)]
pub use wire::run_engine;

/// One compiled-in boot recipe the engine can be told, at `Attach`, to become.
///
/// Each front-end binary — the REPL, exarch — re-execs itself as its own
/// engine and passes its own table; only the tag crosses the wire, never
/// the function.
pub struct EngineInstaller {
    pub tag: &'static str,
    /// Prelude, host surface, libraries, ledger arming — at Attach, once. An
    /// `Err` refuses the attach, in the recipe's own words.
    pub boot: fn(&Attach) -> Result<Booted, String>,
    /// How a seeded or adopted child's grant becomes a ceiling. A field and
    /// not a registered hook: core has no base-tag lexicon to resolve a grant
    /// against, so a host that boots an engine must state the policy its
    /// children are held to.
    pub narrow: GrantNarrower,
}

/// What a recipe boots.
pub struct Booted {
    pub shell: Shell,
    /// Whatever must live as long as the engine — a scratch directory, say.
    pub keep: Box<dyn Send>,
}

/// One session's engine: its shell, the scopes that stop its runs, and the
/// installer it was born from.
pub(crate) struct Engine {
    pub(crate) shell: Shell,
    scopes: Arc<Scopes>,
    installer: &'static EngineInstaller,
    /// Declared after `shell`, so it outlives the shell's teardown.
    _keep: Box<dyn Send>,
}

/// Where a carrier sends one dispatch's events; `false` if it could not.
pub(crate) type Outlet = Arc<dyn Fn(DispatchId, Event) -> bool + Send + Sync>;

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
        (self.outlet)(self.id, Event::Surface(ev.clone()));
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
    /// Become what `attach` names: resolve its installer, boot the recipe,
    /// seat the session's cwd and env, and apply a hatch `seed` if there is
    /// one.
    ///
    /// # Errors
    /// The attach refusal, in the words of whichever step refused.
    pub(crate) fn boot(
        installers: &'static [EngineInstaller],
        attach: &Attach,
        seed: Option<EngineSeed>,
    ) -> Result<Self, String> {
        let installer = resolve_installer(installers, attach.proto_version, &attach.installer)?;
        let Booted { mut shell, keep } = (installer.boot)(attach)?;
        shell.seed_cwd(attach.cwd.clone());
        for (name, value) in &attach.env {
            shell.set_env_var(name, value);
        }
        // Gated on the env var, never the installer tag: only ral-daemon's
        // closed environment sets it, so every recipe is jailed alike.
        #[cfg(target_os = "linux")]
        if std::env::var("RAL_GUEST").is_ok() {
            shell.install_guest_jail(Arc::new(crate::process::jail::GuestJail::new(
                std::path::PathBuf::from("/sys/fs/cgroup/ral-exec"),
                100_000,
                crate::process::jail::JailLimits::default(),
            )));
        }
        if let Some(seed) = seed {
            seed.apply(&mut shell, installer.narrow)?;
        }
        Ok(Self::new(shell, installer, keep))
    }

    /// An engine over a shell already booted.
    pub(crate) fn new(
        shell: Shell,
        installer: &'static EngineInstaller,
        keep: Box<dyn Send>,
    ) -> Self {
        let scopes = Arc::new(Scopes {
            dispatches: shell.run_cancel_handle(),
            root: shell.cancel_handle(),
            slot: Mutex::default(),
        });
        Self {
            shell,
            scopes,
            installer,
            _keep: keep,
        }
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
            run,
            surface: Some(Arc::new(Surface {
                id,
                outlet: rails.outlet,
            })),
            deferred: rails.deferred,
            desk: Some(rails.desk),
            fork: Some(rails.fork),
        };
        self.shell.run_under(scope, req).into_report(&self.shell)
    }

    /// Read one probe class, at a run boundary.
    pub(crate) fn probe(&self, reading: &FOValue) -> Result<FOValue, String> {
        crate::protocol::reading::answer(&self.shell, reading)
    }
}

/// What stops an engine's runs, shared with whichever thread carries its
/// `Control`: each dispatch's scope hangs off `dispatches`, and everything —
/// detached workers too — off `root`.
pub(crate) struct Scopes {
    dispatches: ForegroundScope,
    root: DurableRoot,
    slot: Mutex<Slot>,
}

#[derive(Default)]
struct Slot {
    /// The dispatch `Interrupt` and `Cancel` strike. Replaced, never cleared:
    /// a settled run's scope is dead, so striking it is a no-op, and a stale
    /// `Cancel` names an id the next run does not bear.
    current: Option<(DispatchId, ForegroundScope)>,
    /// A `Cancel` that overtook the dispatch it names.
    foretold: Option<DispatchId>,
}

impl Scopes {
    /// Mint `id`'s scope ahead of its run's frame, so a cancel raised
    /// meanwhile has somewhere to land; struck already if a `Cancel` foretold
    /// it.
    pub(crate) fn open(&self, id: DispatchId) -> ForegroundScope {
        let scope = self.dispatches.child();
        let mut slot = self.slot.lock_ignore_poison();
        if slot.foretold.take_if(|pending| *pending == id).is_some() {
            scope.cancel(CancelCause::Explicit);
        }
        slot.current = Some((id, scope.clone()));
        scope
    }

    /// One `Control` verb, meaning the same under either carrier.
    pub(crate) fn apply(&self, control: Control) {
        match control {
            Control::Interrupt => self.strike(CancelCause::Interrupt),
            Control::Cancel(id) => {
                let mut slot = self.slot.lock_ignore_poison();
                match &slot.current {
                    Some((current, scope)) if *current == id => {
                        scope.cancel(CancelCause::Explicit);
                    }
                    _ => slot.foretold = Some(id),
                }
            }
            Control::Terminate => self.end(CancelCause::Terminate),
            Control::Abort => self.end(CancelCause::RootAbort),
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

    /// Cancel the durable root: every run, and every detached worker.
    pub(crate) fn end(&self, cause: CancelCause) {
        self.root.cancel(cause);
    }
}

/// Engines for core's own tests, booted as every engine is.
#[cfg(test)]
pub(crate) mod testkit {
    use super::{Booted, EngineInstaller, Shell};
    use crate::protocol::{Attach, IdentityTransport, Program, Report, Run};

    /// An installer that never hatches, so it states the one policy a host
    /// with no grant lexicon can: no seeded child.
    pub(crate) const fn installer(
        tag: &'static str,
        boot: fn(&Attach) -> Result<Booted, String>,
    ) -> EngineInstaller {
        EngineInstaller {
            tag,
            boot,
            narrow: |base, _| {
                Err(format!(
                    "this engine hatches no children, so it has no policy to resolve `{base}` by"
                ))
            },
        }
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match EngineInstaller::boot's signature, which can genuinely refuse"
    )]
    fn bare(_attach: &Attach) -> Result<Booted, String> {
        Ok(Booted {
            shell: Shell::new(crate::io::TerminalState::default()),
            keep: Box::new(()),
        })
    }

    /// A shell with no prelude and no surface.
    pub(crate) static BARE: [EngineInstaller; 1] = [installer("bare", bare)];

    /// An attach to `installers`' first recipe, seated in the temp dir.
    pub(crate) fn attach(installers: &[EngineInstaller]) -> Attach {
        let temp = std::env::temp_dir();
        Attach::new(installers[0].tag, temp.clone(), temp)
    }

    /// Boot `attach` in this process.
    pub(crate) fn boot_at(
        installers: &'static [EngineInstaller],
        attach: &Attach,
    ) -> IdentityTransport {
        IdentityTransport::boot(installers, attach).expect("a test recipe boots")
    }

    /// Boot `installers`' first recipe, seated in the temp dir.
    pub(crate) fn boot(installers: &'static [EngineInstaller]) -> IdentityTransport {
        boot_at(installers, &attach(installers))
    }

    /// One capturing run of `src` under the ⊤ capability ceiling.
    pub(crate) fn run(src: &str) -> Run {
        Run {
            program: Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: crate::types::GrantStack::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: crate::run::RunIo::Capture,
            terminal: crate::run::RequestedTerminalAccess::Denied,
            stdin: crate::run::RunStdin::Empty,
            trail: None,
        }
    }

    /// Dispatch `src` under the mute host.
    pub(crate) fn eval(transport: &IdentityTransport, src: &str) -> Report {
        crate::protocol::dispatch_to_report(transport, run(src), std::sync::Arc::new(()))
            .expect("an identity transport answers synchronously")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_installer_matches_known_tag() {
        match resolve_installer(&testkit::BARE, PROTOCOL_VERSION, "bare") {
            Ok(target) => assert_eq!(target.tag, "bare"),
            Err(msg) => panic!("known tag must resolve, got {msg}"),
        }
    }

    #[test]
    fn resolve_installer_refuses_unknown_tag() {
        match resolve_installer(&testkit::BARE, PROTOCOL_VERSION, "no-such-installer") {
            Ok(_) => panic!("unknown tag must be refused"),
            Err(msg) => {
                assert!(msg.contains("unknown builtin installer"));
                assert!(msg.contains("no-such-installer"));
            }
        }
    }

    #[test]
    fn resolve_installer_refuses_protocol_mismatch_before_tag_lookup() {
        match resolve_installer(&testkit::BARE, PROTOCOL_VERSION + 1, "bare") {
            Ok(_) => panic!("a mismatched protocol version must be refused"),
            Err(msg) => assert!(msg.contains("protocol version mismatch")),
        }
    }
}
