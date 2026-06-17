//! The host-embedding accessor surface.
//!
//! [`Shell::turn`](super::Shell), `session`, and `local` are `pub(crate)`:
//! the fields that encode turn safety are not a public API, so a host cannot
//! reach in and install an unrelated foreground scope, clear the source
//! registry mid-turn, or swap a stream behind the turn guard's back.  Hosts
//! drive a session through the narrow operations gathered here — the ones the
//! REPL and exarch actually need — while [`Shell::mobile`](super::Shell) stays
//! the public embedding seam.

use super::repl::ReplScratch;
use super::{Shell, SurfaceSink};
use crate::diagnostic::SourceDb;
use crate::exit_hints::ExitHints;
use crate::io::{Sink, TerminalState};
use crate::process::{DurableRoot, ForegroundScope};
use crate::types::AuditFragment;

impl Shell {
    /// The session's durable source registry.  Hosts read it after a turn
    /// returns to render a runtime error against the right source text.
    pub fn sources(&self) -> &SourceDb {
        &self.session.sources
    }

    /// The session's durable cancel root.  A host mints a turn's foreground
    /// scope from it (`shell.durable_root().child()`); the typed relation
    /// keeps that scope rooted here.
    pub fn durable_root(&self) -> &DurableRoot {
        &self.session.root
    }

    /// The current turn's foreground scope.  A host clones a deadline child
    /// of it, or cancels it to interrupt the foreground work.
    pub fn foreground(&self) -> &ForegroundScope {
        &self.turn.cancel
    }

    /// Install the startup-loaded exit-code hint table.
    pub fn set_exit_hints(&mut self, hints: ExitHints) {
        self.session.exit_hints = hints;
    }

    /// Cached terminal state probed at startup (isatty / ANSI / mode bits).
    /// `Copy`, so frontends read the bits they need without borrowing.
    pub fn terminal(&self) -> TerminalState {
        self.turn.io.terminal
    }

    /// Whether the shell is running as an interactive REPL.
    pub fn is_interactive(&self) -> bool {
        self.turn.io.interactive
    }

    /// Mark the shell interactive (or not).  The interactive REPL sets this
    /// at boot so external commands and prompts behave as a live session.
    pub fn set_interactive(&mut self, interactive: bool) {
        self.turn.io.interactive = interactive;
    }

    /// Install the session stdout sink.  The interactive REPL installs its
    /// `ExternalPrinter` here so background output lands above the prompt.
    pub fn set_stdout(&mut self, stdout: Sink) {
        self.turn.io.stdout = stdout;
    }

    /// The session stderr sink, for a host to write diagnostics into.
    pub fn stderr_mut(&mut self) -> &mut Sink {
        &mut self.turn.io.stderr
    }

    /// Install a structured-event sink for the `surface` builtin to forward
    /// to.  An `Inherit`-regime host (the REPL) sets one on the session and a
    /// turn carries it forward; a `Capture`-regime host installs it per turn
    /// through the frame instead.
    pub fn set_surface(&mut self, sink: SurfaceSink) {
        self.turn.surface = Some(sink);
    }

    /// Turn on top-level audit collection with byte capture (`ral --audit`).
    /// SPEC §10.3: every emitted command node carries stdout/stderr, so the
    /// trail is installed under `CapturePolicy::Bytes` — mirroring the
    /// `audit { … }` builtin, not the default `None` policy that would leave
    /// those fields empty.
    pub fn enable_audit(&mut self) {
        self.local
            .audit
            .install_active_policy(Some(crate::types::CapturePolicy::Bytes));
    }

    /// Drain the accumulated audit trail as a fragment (e.g. for `--audit`
    /// JSON output at end of run).
    pub fn take_audit_fragment(&mut self) -> AuditFragment {
        self.local.audit.take_fragment()
    }

    /// The names of every installed builtin command, for tab completion.
    pub fn builtin_names(&self) -> impl Iterator<Item = &str> {
        self.session.builtins.names()
    }

    /// Read-only access to REPL/editor scratch (plugin context, TUI flag,
    /// queued chpwd notification).
    pub fn repl(&self) -> &ReplScratch {
        &self.local.repl
    }

    /// Mutable access to REPL/editor scratch.  This is the REPL host's own
    /// state; exposing it to the host that owns it is the seam, not a leak.
    pub fn repl_mut(&mut self) -> &mut ReplScratch {
        &mut self.local.repl
    }
}
