//! The one shell with no parent.

use super::{Context, LocalState, SessionState, Shell};
use crate::capability::GrantStack;
use crate::types::{Env, Signature};
use std::sync::Arc;

impl Shell {
    /// A parentless shell over the given terminal state, with nothing
    /// installed: no builtins, no env vars, no prelude.  A host picks its
    /// surface through [`HostSurface::shell`](crate::HostSurface::shell), so no
    /// shell silently lacks one.
    ///
    /// Terminal flags are explicit so a caller cannot leave them all-false,
    /// which would show external commands piped I/O in place of the real
    /// terminal.  The session faces no signals: its host forwards them as
    /// `Control`.
    pub(crate) fn root(terminal: crate::terminal::TerminalState) -> Self {
        let root = crate::process::DurableRoot::new();
        Self {
            env: Env::new(),
            sig: Arc::new(Signature::default()),
            context: Context {
                grants: GrantStack::root(),
                ..Context::default()
            },
            io: crate::io::Io {
                terminal,
                ..Default::default()
            },
            session: SessionState {
                anchor: root.worker(),
                root,
                sources: crate::source::SourceDb::default(),
                root_file: None,
                exit_hints: crate::types::ExitHints::default(),
                builtins: crate::types::BuiltinTable::default(),
                library_docs: std::collections::HashMap::new(),
                terminal_lease: crate::process::TerminalLease::mint_at_startup(
                    terminal.startup_foreground,
                ),
                guest_jail: None,
                stack_limit: super::DEFAULT_STACK_LIMIT,
                hooks: std::collections::HashMap::new(),
            },
            local: LocalState::default(),
        }
    }
}
