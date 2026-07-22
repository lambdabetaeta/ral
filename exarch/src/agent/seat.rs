//! The agent's seat: the transport its turns run through, plus the
//! host-side state that seat kind owns.  Every engine-side reach — per-call
//! installs, `/clear`'s rebuild, the registry's cancel reach — is a seat
//! method, so a second seat kind (the wire engine) is one more variant
//! here, never a second copy of the agent.

use crate::agent::event::AgentLog;
use crate::bootstrap::Scratch;
use crate::fleet::desk::{AbsentDesk, DeskBinding, ExarchDesk, SurfaceApplier};
use crate::fleet::registry::{EvalReach, TurnScope};
use crate::shell_eval::builtins;
use ral_core::Shell;
use ral_core::transport::{IdentityTransport, Transport};
use std::sync::{Arc, Mutex};

/// One agent's engine-side attachment.  A closed enum, not a trait object:
/// the operations that differ per seat live off the `Transport` trait.
pub(crate) enum Seat {
    /// In-process: owns the session [`Scratch`] (`/clear` reboots from it)
    /// and the turn-scope cell the registry interrupts through, which must
    /// stay live across a `/clear` rebuild.
    Identity {
        transport: IdentityTransport,
        scratch: Arc<Scratch>,
        turn_scope: TurnScope,
    },
}

/// One `ral` call's capture set, installed for the extent of the eval —
/// built fresh per call so nothing a desk handler captures can go stale.
pub(crate) struct TurnInstall {
    pub(crate) desk: Arc<ExarchDesk>,
    pub(crate) apply: SurfaceApplier,
    pub(crate) deferred: Arc<dyn ral_core::types::DeferredSink>,
    pub(crate) nursery: ral_core::types::Nursery,
}

/// Retires the install on drop — [`AbsentDesk`] back in, nursery cleared —
/// on *every* exit, including a panic `Agent::drive` recovers from, where
/// straight-line teardown would leave the desk's whole capture (its
/// `Emitter` included) installed for the rest of the session.
pub(crate) struct TurnGuard<'s>(&'s Seat);

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        let Seat::Identity { transport, .. } = self.0;
        transport.set_desk(Arc::new(AbsentDesk));
        transport.clear_nursery();
    }
}

impl Seat {
    /// The identity ceremony — trunk construction, every fork, and the
    /// desk's spawn spine all route here; `/clear` re-runs it through
    /// [`Self::clear`] onto the same turn-scope cell.
    pub(crate) fn identity(shell: Shell, scratch: Arc<Scratch>, log: &AgentLog) -> Self {
        let turn_scope: TurnScope = Arc::new(Mutex::new(None));
        let transport = identity_ceremony(shell, log, &turn_scope);
        Self::Identity {
            transport,
            scratch,
            turn_scope,
        }
    }

    /// The transport a dispatch or probe runs against.
    pub(crate) fn transport(&self) -> &dyn Transport {
        let Self::Identity { transport, .. } = self;
        transport
    }

    /// Direct engine-state access, identity-only — the test suite's state
    /// inspection door and `fork_with`'s `fork_session` reach.
    pub(crate) fn shell_mut(&self) -> std::sync::MutexGuard<'_, ral_core::transport::EngineInner> {
        let Self::Identity { transport, .. } = self;
        transport.shell_mut()
    }

    /// Install one call's capture set; the guard retires it on every exit.
    pub(crate) fn install_turn(&self, install: TurnInstall) -> TurnGuard<'_> {
        let Self::Identity { transport, .. } = self;
        transport.set_deferred_sink(install.deferred);
        transport.set_nursery(install.nursery);
        // The drain-then-handle adapter: a handler's chrome must never jump
        // ahead of surface output still queued on the event channel.
        transport.set_desk(Arc::new(DeskBinding {
            desk: install.desk,
            events: transport.events_shared(),
            apply: install.apply,
        }));
        TurnGuard(self)
    }

    /// The cancel reach the fleet registry stores for this agent.
    pub(crate) fn eval_reach(&self) -> EvalReach {
        let Self::Identity {
            transport,
            turn_scope,
            ..
        } = self;
        EvalReach::Identity {
            eval_root: transport.shell_mut().shell.cancel_handle(),
            turn_scope: turn_scope.clone(),
        }
    }

    /// `/clear`'s engine half: reboot the shell from the owned scratch and
    /// re-run the ceremony onto the *same* turn-scope cell.  Replacing the
    /// transport drops the outgoing shell, whose teardown cancels its
    /// registered workers — `/clear` outranks every lease.
    pub(crate) fn clear(&mut self, log: &AgentLog) {
        let Self::Identity {
            transport,
            scratch,
            turn_scope,
        } = self;
        *transport = identity_ceremony(boot_root_shell(scratch), log, turn_scope);
    }
}

/// Boot a root session shell: the shared exarch boot plus the scratch's
/// env/binding seeding.  Forks instead snapshot their parent through
/// `Shell::fork_session`, inheriting the seeding.
pub(crate) fn boot_root_shell(scratch: &Scratch) -> Shell {
    let mut shell = crate::bootstrap::boot_shell();
    scratch.install_into(&mut shell);
    shell
}

/// Seed the session-dir variable, arm the ledgers over everything just
/// seeded (seeding then arming stay one visible sequence —
/// `decisions/260629_agent-binding-reaping`), then seat the shell behind a
/// fresh transport observing `turn_scope` and attach the host endpoint.
fn identity_ceremony(
    mut shell: Shell,
    log: &AgentLog,
    turn_scope: &TurnScope,
) -> IdentityTransport {
    // `EXARCH_SESSION_DIR` must always point at the live session's
    // event-log directory, on construction and every `/clear` rebuild.
    crate::bootstrap::seed_var(
        &mut shell,
        "EXARCH_SESSION_DIR",
        &log.dir().to_string_lossy(),
    );
    crate::bootstrap::arm_session_ledgers(&mut shell);
    let mut transport = IdentityTransport::new(shell);
    transport.observe_foreground(turn_scope.clone());
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    transport.attach(
        ral_core::transport::TerminalEndpoint {
            lease: None,
            state: crate::bootstrap::probe_terminal(),
        },
        cwd,
        std::path::PathBuf::from(&home),
        None, // rc_path
        builtins::INSTALLER_TAG.to_string(),
    );
    transport
}
