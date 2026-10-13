//! Moving state from a parent shell into a child computation.
//!
//! Spawned thread, pipeline stage, REPL aside and sub-agent
//! session are genuine forks over a different store, and all four are built by
//! the one literal in [`Shell::child`]: a field added to [`Shell`] cannot reach
//! a child by defaulting.  No child mints a
//! [`TerminalLease`](crate::process::TerminalLease): the foreground gate wants
//! the run's access *and* the session's lease, so a fork fails the second half
//! whatever [`Mooring`] it later runs under.

use super::bindings::BindingLedger;
use super::detached::DetachPolicy;
use super::workers::Roster;
use super::{LocalState, Mooring, SessionState, Shell};
use crate::io::{Io, Source};
use crate::process::{DurableRoot, ForegroundScope};
use crate::ty::Fixings;
use crate::types::Audit;
use std::sync::Arc;

/// What the four kinds of child differ in.
struct Holding {
    root: DurableRoot,
    anchor: ForegroundScope,
    workers: Roster,
    bindings: BindingLedger,
    detach: Option<Arc<DetachPolicy>>,
    fixings: Arc<Fixings>,
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
            anchor,
            workers,
            bindings,
            detach,
            fixings,
        } = holding;
        Self {
            env: self.env.clone(),
            sig: Arc::clone(&self.sig),
            context: self.context.clone(),
            io: Io {
                stdin: Source::Empty,
                ..Io::default()
            },
            session: SessionState {
                root,
                anchor,
                sources: self.session.sources.clone(),
                root_file: self.session.root_file,
                exit_hints: Arc::clone(&self.session.exit_hints),
                fixings,
                builtins: self.session.builtins.clone(),
                library_docs: self.session.library_docs.clone(),
                terminal_lease: None,
                guest_jail: self.session.guest_jail.clone(),
                stack_limit: self.session.stack_limit,
                hooks: None,
            },
            local: LocalState {
                audit: Audit::dispatched_from(self.local.audit.call_site),
                workers,
                bindings,
                detach,
                machine_depth: 0,
            },
        }
    }

    /// Fork this shell into an independent child *session* — the primitive a
    /// host uses to spawn a sub-agent that executes its own runs.
    ///
    /// A new session keeps its parent's policy (exit hints, the lease's terms
    /// with everything inherited sealed as baseline, worker retention, the
    /// detach budget's size) and a snapshot of its parent's fixings, and
    /// answers for none of its parent's state: a fresh root deaf to the
    /// ambient causes (its host cancels it through [`Shell::cancel_handle`]),
    /// a roster of its own, an unspent detach budget.  Nothing flows back:
    /// the child's `cd`, env, and new bindings die with it.
    pub fn fork_session(&self) -> Self {
        let root = DurableRoot::new();
        let anchor = root.worker();
        self.child(Holding {
            root,
            anchor,
            workers: self.local.workers.fresh(),
            bindings: self.local.bindings.forked(|| self.visible_names()),
            detach: self
                .local
                .detach
                .as_deref()
                .map(|policy| Arc::new(policy.fresh())),
            fixings: Arc::new(self.session.fixings.snapshot()),
        })
    }

    /// Join this session as an *aside*: a second [`Shell`] the host runs
    /// beside it rather than as one — the REPL's hook shell, evaluating
    /// arbitrary plugin code while the session sits at its prompt.
    ///
    /// The twin of [`Self::fork_session`], opposite in the one place that
    /// matters: it *shares* this session's durable root instead of minting a
    /// fresh one, so it is inside the session for cancellation.  An interrupt
    /// aimed at a command the session was already running is older than every
    /// frame the aside will mint, so the aside can neither absorb it nor keep
    /// it from the run it was aimed at.
    pub fn join_session(&self) -> Self {
        let root = self.session.root.clone();
        let anchor = root.worker();
        self.child(Holding {
            root,
            anchor,
            workers: self.local.workers.fresh(),
            bindings: BindingLedger::default(),
            detach: self.local.detach.clone(),
            fixings: Arc::clone(&self.session.fixings),
        })
    }

    /// The one and only thread-spawn primitive: `f` runs on a fresh OS thread
    /// with a child shell built here, on the parent's thread. `mooring` is
    /// minted by the caller, since only it has the parent mooring to rebuild
    /// from.
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
    ) -> std::io::Result<(std::thread::JoinHandle<R>, crate::process::CancelScope)>
    where
        F: FnOnce(&Mooring, &mut Self) -> R + Send + 'static,
        R: Send + 'static,
    {
        let mut child = self.child(Holding {
            root: self.session.root.clone(),
            anchor: mooring.cancel.clone(),
            workers: self.local.workers.share(),
            bindings: BindingLedger::default(),
            detach: self.local.detach.clone(),
            fixings: Arc::clone(&self.session.fixings),
        });
        let worker_cancel = mooring.cancel.as_scope().clone();
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
        Ok((handle, worker_cancel))
    }
}

// Unix-only: the lease tests assert a minted `TerminalLease` is `Some`, and
// `mint_at_startup` returns `None` unconditionally where there is no
// `tcsetpgrp`.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::process::TerminalLease;
    use crate::types::shell::{DEFAULT_STACK_LIMIT, TerminalAccess};
    use crate::types::{DefaultPolicy, HookName, HookSig, block_over};

    /// Hooks are session state: a worker, a stage, an aside and a fork each
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
        for child in [parent.fork_session(), parent.join_session()] {
            assert!(!child.has_hook(&HookName::session("prompt")));
            assert!(matches!(child.io.stdin, crate::io::Source::Empty));
        }
        let (join, _cancel) = parent
            .spawn_thread(Mooring::adrift(), "test-worker", |_, child| {
                (
                    child.has_hook(&HookName::session("prompt")),
                    matches!(child.io.stdin, crate::io::Source::Empty),
                )
            })
            .expect("spawn_thread");
        assert_eq!(join.join().expect("worker thread"), (false, true));
    }

    /// A forked session builds over a defaulted `SessionState` and so mints no
    /// lease witness: a sub-agent can never foreground an external command and
    /// seize the controlling terminal the host's TUI owns, even forked from a
    /// parent that holds the lease and run under a mooring claiming `Leased`.
    #[test]
    fn fork_session_holds_no_terminal_authority() {
        let mut parent = crate::test_helper::core_shell();
        parent.session.terminal_lease = TerminalLease::mint_at_startup(true);
        let mooring = Mooring {
            terminal_access: TerminalAccess::Leased,
            ..Mooring::adrift()
        };
        assert!(
            parent.terminal_lease(&mooring).is_some(),
            "precondition: the parent holds a Leased lease",
        );

        let child = parent.fork_session();
        assert!(
            child.terminal_lease(&mooring).is_none(),
            "a forked session minted no lease witness, so it cannot foreground",
        );
    }

    /// An rc `recursion-limit:` key or a `--recursion-limit` flag configures
    /// the session as `session.stack_limit`, so a `spawn` / `par` / `watch`
    /// body must not silently fall back to the compile-time default.
    #[test]
    fn spawned_worker_inherits_the_stack_limit() {
        let mut parent = crate::test_helper::core_shell();
        parent.set_stack_limit(DEFAULT_STACK_LIMIT + 7);
        let (join, _cancel) = parent
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
        let (join, _cancel) = parent
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
        let (join, _cancel) = parent
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
        let (join, _cancel) = parent
            .spawn_thread(Mooring::adrift(), "test-worker", |_, child| {
                Arc::clone(&child.session.exit_hints)
            })
            .expect("spawn_thread");

        let hints = join.join().expect("worker thread");
        assert!(Arc::ptr_eq(&hints, &parent.session.exit_hints));
    }

    /// A forked session keeps its parent's policy and answers for none of its
    /// state.
    #[test]
    fn a_forked_session_keeps_the_policy_and_answers_for_nothing() {
        let mut parent = crate::test_helper::core_shell();
        parent.set_exit_hints(crate::types::ExitHints::from_text("false 1 hinted\n"));
        parent.arm_binding_lease(crate::types::BindingLease {
            idle_calls: 3,
            large_binding_bytes: 1 << 20,
        });
        parent.arm_worker_retention(5);
        parent.arm_detach(4);
        parent
            .local
            .detach
            .as_ref()
            .expect("armed")
            .admit()
            .expect("a birth is left")
            .commit();
        parent.set_var("late".to_string(), crate::types::Value::Int(1));

        let child = parent.fork_session();
        assert!(Arc::ptr_eq(
            &parent.session.exit_hints,
            &child.session.exit_hints
        ));
        assert!(child.local.bindings.armed());
        assert_eq!(child.local.bindings.leased_count(), 0);
        assert!(child.local.bindings.exempts("late"));
        assert_eq!(child.local.workers.retention(), Some(5));
        let (theirs, ours) = (
            child.local.detach.as_ref().expect("armed"),
            parent.local.detach.as_ref().expect("armed"),
        );
        assert_eq!(theirs.budget, 4);
        assert_eq!(theirs.births.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(!Arc::ptr_eq(theirs, ours));
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

    /// A forked session starts from its parent's fixings and writes only its
    /// own.
    #[test]
    fn a_forked_session_snapshots_its_fixings() {
        use crate::ty::{Side, UnitId};
        let unit = UnitId::fresh();
        let parent = crate::test_helper::core_shell();
        assert!(parent.fixings().fix(unit, 0, Side::Number, "/a").is_ok());
        let child = parent.fork_session();
        assert!(child.fixings().fix(unit, 0, Side::Text, "/b").is_err());
        assert!(child.fixings().fix(unit, 1, Side::Text, "/b").is_ok());
        assert!(parent.fixings().fix(unit, 1, Side::Number, "/b").is_ok());
    }

    /// An aside is the same session: it shares the fixings.
    #[test]
    fn an_aside_shares_the_session_fixings() {
        let parent = crate::test_helper::core_shell();
        let aside = parent.join_session();
        assert!(Arc::ptr_eq(&parent.session.fixings, &aside.session.fixings));
    }
}
