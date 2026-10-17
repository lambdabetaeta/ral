//! Moving state from a parent shell into a child computation.
//!
//! Spawned thread, pipeline stage and REPL aside share a session's memory, and
//! all three are built by the one literal in [`Shell::child`]: a field added to
//! [`Shell`] cannot reach a child by defaulting.  (A sub-agent is a session of
//! its own, built by `Shell::child_of`.)  No child mints a
//! [`TerminalLease`](crate::process::TerminalLease): the foreground gate wants
//! the run's access *and* the session's lease, so a fork fails the second half
//! whatever [`Mooring`] it later runs under.

use super::bindings::BindingLedger;
use super::workers::Roster;
use super::{LocalState, Mooring, SessionState, Shell};
use crate::io::{Io, Source};
use crate::process::{DurableRoot, Scope};
use crate::types::Audit;
use std::sync::Arc;

/// What the three kinds of child differ in.
struct Holding {
    root: DurableRoot,
    /// The child's session anchor and its current scope: one node at birth.
    scope: Scope,
    workers: Roster,
}

impl Shell {
    /// This shell's child, built on the parent's thread: it inherits the
    /// lexical scope, context, Σ, builtin table, source registry, call site
    /// and the session's exit hints, so it resolves, renders and describes as
    /// the parent does, and differs from its siblings only in its
    /// [`Holding`].  Everything else is fresh: control counters, since a child
    /// continues no call stack; no hooks, since a worker or a stage dispatches
    /// none; stdin `Empty`, since a child must not `tcgetpgrp` whoever owns
    /// the real terminal.
    fn child(&self, holding: Holding) -> Self {
        let Holding {
            root,
            scope,
            workers,
        } = holding;
        Self {
            env: self.env.clone(),
            sig: Arc::clone(&self.sig),
            context: self.context.clone(),
            io: Io {
                stdin: Source::Empty,
                ..Io::default()
            },
            scope: scope.clone(),
            session: SessionState {
                root,
                anchor: scope,
                sources: self.session.sources.clone(),
                root_file: self.session.root_file,
                exit_hints: Arc::clone(&self.session.exit_hints),
                fixings: Arc::clone(&self.session.fixings),
                runtime: Arc::clone(&self.session.runtime),
                terminal_lease: None,
                stack_limit: self.session.stack_limit,
                hooks: None,
            },
            local: LocalState {
                audit: Audit::dispatched_from(self.local.audit.call_site),
                workers,
                bindings: BindingLedger::default(),
                detach: self.local.detach.clone(),
                machine_depth: 0,
            },
        }
    }

    /// Join this session as an *aside*: a second [`Shell`] the host runs
    /// beside it rather than as one — the REPL's hook shell, evaluating
    /// arbitrary plugin code while the session sits at its prompt.
    ///
    /// It *shares* this session's durable root rather than minting a fresh
    /// one, so it is inside the session for cancellation.  An interrupt
    /// aimed at a command the session was already running is older than every
    /// frame the aside will mint, so the aside can neither absorb it nor keep
    /// it from the run it was aimed at.
    pub fn join_session(&self) -> Self {
        let root = self.session.root.clone();
        let scope = Scope::clone(&root.worker());
        self.child(Holding {
            root,
            scope,
            workers: self.local.workers.fresh(),
        })
    }

    /// The one and only thread-spawn primitive: `f` runs on a fresh OS thread
    /// with a child shell built here, on the parent's thread. `mooring` is
    /// minted by the caller, since only it has the parent mooring to rebuild
    /// from; the caller minted its node too, and keeps it.
    ///
    /// The worker registry is `Arc`-shared, not copied, so a nested `spawn`
    /// registers alongside its parent, and the worker's shell dropping cancels
    /// nothing of the parent's.
    ///
    /// # Errors
    /// Returns `Err` if the OS refuses to start the thread.
    pub(crate) fn spawn_thread<F, R>(
        &self,
        mooring: Mooring,
        name: &str,
        f: F,
    ) -> std::io::Result<std::thread::JoinHandle<R>>
    where
        F: FnOnce(&Mooring, &mut Self) -> R + Send + 'static,
        R: Send + 'static,
    {
        let mut child = self.child(Holding {
            root: self.session.root.clone(),
            scope: Scope::clone(&mooring.home),
            workers: self.local.workers.share(),
        });
        let live = self.local.workers.live_ticket();
        let handle = std::thread::Builder::new()
            .name(name.into())
            .stack_size(8 << 20)
            .spawn(move || {
                let out = f(&mooring, &mut child);
                // The ticket goes last, after the body and its shell, so a
                // teardown's drain outlasts this frame's children rather than
                // merely seeing the cancel land.
                drop((child, live));
                out
            })?;
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::shell::DEFAULT_STACK_LIMIT;
    use crate::types::{DefaultPolicy, HookName, HookSig, block_over};

    /// Hooks are session state: a worker, a stage and an aside each
    /// start without them, and with an `Empty` stdin, so none can foreground
    /// whoever owns the real terminal.
    #[test]
    fn a_child_holds_no_hooks_and_reads_no_terminal() {
        let mut parent = crate::test_helper::core_shell();
        let body = block_over(&parent.env);
        parent
            .register_hook(
                HookName::session("prompt"),
                body,
                HookSig::Block,
                DefaultPolicy::denied(),
            )
            .expect("register a hook");
        assert!(parent.has_hook(&HookName::session("prompt")));
        let aside = parent.join_session();
        assert!(!aside.has_hook(&HookName::session("prompt")));
        assert!(matches!(aside.io.stdin, crate::io::Source::Empty));
        let join = parent
            .spawn_thread(Mooring::adrift(), "test-worker", |_, child| {
                (
                    child.has_hook(&HookName::session("prompt")),
                    matches!(child.io.stdin, crate::io::Source::Empty),
                )
            })
            .expect("spawn_thread");
        assert_eq!(join.join().expect("worker thread"), (false, true));
    }

    /// An rc `recursion-limit:` key or a `--recursion-limit` flag configures
    /// the session as `session.stack_limit`, so a `spawn` / `par` / `watch`
    /// body must not silently fall back to the compile-time default.
    #[test]
    fn spawned_worker_inherits_the_stack_limit() {
        let mut parent = crate::test_helper::core_shell();
        parent.set_stack_limit(DEFAULT_STACK_LIMIT + 7);
        let join = parent
            .spawn_thread(Mooring::adrift(), "test-worker", |_, child| {
                child.session.stack_limit
            })
            .expect("spawn_thread");

        let stack_limit = join.join().expect("worker thread");
        assert_eq!(stack_limit, DEFAULT_STACK_LIMIT + 7);
    }

    /// `session.sources` must ride into a spawned worker's shell, else a
    /// `spawn` body's error span resolves against nothing and the diagnostic
    /// falls back to caret-less rendering.
    #[test]
    fn spawned_worker_renders_errors_against_the_parents_source() {
        let mut parent = crate::test_helper::core_shell();
        let file = parent.install_script_context("worker.ral", "one\ntwo\nbad\n");
        let span = crate::source::Span::new(file, 8, 11);
        let join = parent
            .spawn_thread(Mooring::adrift(), "test-worker", move |_, child| {
                crate::types::Error::new("boom")
                    .at_span(span)
                    .render(&child.session.sources, None)
            })
            .expect("spawn_thread");

        let rendered = join.join().expect("worker thread");
        assert!(
            rendered.contains("worker.ral"),
            "the worker's error must resolve against the parent's source: {rendered}"
        );
    }

    /// `session.root_file` must ride into a spawned worker's shell alongside
    /// `sources`, else a bare `source`/`use` in a `defer`/`spawn` body or a
    /// pipeline stage — which starts no load of its own — falls back through
    /// a missing registry entry to cwd-relative resolution instead of the
    /// run's own root, and can silently load the wrong file.
    #[test]
    fn spawned_worker_inherits_the_root_file() {
        let mut parent = crate::test_helper::core_shell();
        parent.install_root_context("main.ral", "");
        let root_file = parent.session.root_file;
        let join = parent
            .spawn_thread(Mooring::adrift(), "test-worker", |_, child| {
                child.session.root_file
            })
            .expect("spawn_thread");

        let child_root_file = join.join().expect("worker thread");
        assert_eq!(
            child_root_file, root_file,
            "the worker must inherit the parent's root file"
        );
    }

    /// A worker or a stage hints at a failed command as its session does.
    #[test]
    fn spawned_worker_shares_the_exit_hints() {
        let mut parent = crate::test_helper::core_shell();
        parent.set_exit_hints(crate::types::ExitHints::from_text("false 1 hinted\n"));
        let join = parent
            .spawn_thread(Mooring::adrift(), "test-worker", |_, child| {
                Arc::clone(&child.session.exit_hints)
            })
            .expect("spawn_thread");

        let hints = join.join().expect("worker thread");
        assert!(Arc::ptr_eq(&hints, &parent.session.exit_hints));
    }

    /// An aside is the same session: it shares the detach policy and leases
    /// nothing of its own.
    #[test]
    fn an_aside_shares_the_detach_policy_and_leases_nothing() {
        let mut parent = crate::test_helper::core_shell();
        parent.arm_detach(4);
        parent.arm_binding_lease(crate::types::BindingLease {
            idle_calls: 3,
            large_binding_bytes: 1 << 20,
        });
        let aside = parent.join_session();
        assert!(Arc::ptr_eq(
            aside.local.detach.as_ref().expect("armed"),
            parent.local.detach.as_ref().expect("armed"),
        ));
        assert!(!aside.local.bindings.armed());
    }

    /// An aside is the same session: it shares the fixings.
    #[test]
    fn an_aside_shares_the_session_fixings() {
        let parent = crate::test_helper::core_shell();
        let aside = parent.join_session();
        assert!(Arc::ptr_eq(&parent.session.fixings, &aside.session.fixings));
    }
}
