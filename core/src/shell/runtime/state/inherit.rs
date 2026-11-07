//! Moving state from a parent shell into a child computation.
//!
//! Spawned worker, pipeline stage and REPL aside share a session's memory, and
//! all three are built by the one literal in [`Shell::child`]: a field added to
//! [`Shell`] cannot reach a child by defaulting, and each birth states its
//! child's streams and place in a [`Seat`].  (A sub-agent is a session of
//! its own, built by `Shell::child_of`.)  No child mints a
//! [`TerminalLease`](crate::process::TerminalLease): the foreground gate wants
//! the run's access *and* the session's lease, so a fork fails the second half
//! whatever [`Mooring`] it later runs under.

use super::bindings::BindingLedger;
use super::workers::Roster;
use super::{LocalState, Mooring, SessionState, Shell};
use crate::io::{Io, Sink, Source, Streams};
use crate::process::{DurableRoot, Membership, Scope};
use crate::shell::runtime::{Audit, Cradle};
use std::sync::Arc;

/// What the three kinds of child differ in.
struct Holding {
    root: DurableRoot,
    /// The child's session anchor and its current scope: one node at birth.
    scope: Scope,
    workers: Roster,
    seat: Seat,
}

/// A child's streams and its place among the session's processes.
struct Seat {
    io: Io,
    interactive: bool,
    stage: Option<Membership>,
}

impl Shell {
    /// This shell's child, built on the parent's thread: it inherits the
    /// lexical scope, context, Σ, builtin table, source registry, call site,
    /// and the session's exit hints and scratch, so it resolves, renders and
    /// describes as the parent does, and differs from its siblings only in its
    /// [`Holding`].  Everything else is fresh: control counters, since a child
    /// continues no call stack; no hooks, since a worker or a stage dispatches
    /// none.
    fn child(&self, holding: Holding) -> Self {
        let Holding {
            root,
            scope,
            workers,
            seat:
                Seat {
                    io,
                    interactive,
                    stage,
                },
        } = holding;
        Self {
            env: self.env.clone(),
            sig: Arc::clone(&self.sig),
            context: self.context.clone(),
            io,
            scope: scope.clone(),
            session: SessionState {
                root,
                anchor: scope,
                sources: self.session.sources.fork(),
                root_file: self.session.root_file,
                exit_hints: Arc::clone(&self.session.exit_hints),
                scratch: self.session.scratch.clone(),
                fixings: Arc::clone(&self.session.fixings),
                runtime: Arc::clone(&self.session.runtime),
                terminal_lease: None,
                stack_limit: self.session.stack_limit,
                child_limits: self.session.child_limits,
                interactive,
                stage,
                hooks: None,
            },
            local: LocalState {
                audit: Audit::dispatched_from(self.local.audit.call_site().clone()),
                workers,
                cradle: self.local.cradle.clone(),
                bindings: BindingLedger::default(),
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
    ///
    /// It writes where the session does and reads nothing, so it cannot
    /// `tcgetpgrp` whoever owns the real terminal.
    pub fn join_session(&self) -> Self {
        let root = self.session.root.clone();
        let scope = Scope::clone(&root.worker());
        let cradle = Cradle::loose(Scope::clone(&root));
        let mut aside = self.child(Holding {
            root,
            scope,
            workers: self.local.workers.fresh(),
            seat: Seat {
                io: Io {
                    stdin: Source::Empty,
                    stdout: self.io.stdout.clone(),
                    stderr: self.io.stderr.clone(),
                },
                interactive: self.session.interactive,
                stage: self.session.stage.clone(),
            },
        });
        aside.local.cradle = cradle;
        aside
    }

    /// A worker's thread: it reads nothing, writes `out`, and joins no
    /// group, so its externals lead sessions of their own and none can steal
    /// interactive input.
    ///
    /// # Errors
    /// Returns `Err` if the OS refuses to start the thread.
    pub(crate) fn spawn_worker<F, R>(
        &self,
        mooring: Mooring,
        out: Streams<Sink>,
        f: F,
    ) -> std::io::Result<std::thread::JoinHandle<R>>
    where
        F: FnOnce(&Mooring, &mut Self) -> R + Send + 'static,
        R: Send + 'static,
    {
        let Streams { stdout, stderr } = out;
        let seat = Seat {
            io: Io {
                stdin: Source::Empty,
                stdout,
                stderr,
            },
            interactive: false,
            stage: None,
        };
        self.spawn_thread(mooring, seat, "ral spawn worker", f)
    }

    /// A pipeline stage's thread, on `io`, its externals joining `membership`.
    ///
    /// # Errors
    /// Returns `Err` if the OS refuses to start the thread.
    pub(crate) fn spawn_stage<F, R>(
        &self,
        mooring: Mooring,
        io: Io,
        membership: Membership,
        f: F,
    ) -> std::io::Result<std::thread::JoinHandle<R>>
    where
        F: FnOnce(&Mooring, &mut Self) -> R + Send + 'static,
        R: Send + 'static,
    {
        let seat = Seat {
            io,
            interactive: self.session.interactive,
            stage: Some(membership),
        };
        self.spawn_thread(mooring, seat, "ral pipeline stage", f)
    }

    /// The one and only thread-spawn primitive: `f` runs on a fresh OS thread
    /// with a child shell built here, on the parent's thread. `mooring` is
    /// minted by the caller, since only it has the parent mooring to rebuild
    /// from; the caller minted its node too, and keeps it.
    ///
    /// The worker registry is `Arc`-shared, not copied, so a nested `spawn`
    /// registers alongside its parent, and the worker's shell dropping cancels
    /// nothing of the parent's.  The cradle is the parent's too: a stage keeps
    /// it, a worker's base replaces it with its own.
    fn spawn_thread<F, R>(
        &self,
        mooring: Mooring,
        seat: Seat,
        name: &str,
        f: F,
    ) -> std::io::Result<std::thread::JoinHandle<R>>
    where
        F: FnOnce(&Mooring, &mut Self) -> R + Send + 'static,
        R: Send + 'static,
    {
        let child = self.child(Holding {
            root: self.session.root.clone(),
            scope: Scope::clone(&mooring.home),
            workers: self.local.workers.share(),
            seat,
        });
        let live = self.local.workers.live_ticket();
        let handle = std::thread::Builder::new()
            .name(name.into())
            .stack_size(8 << 20)
            .spawn(move || {
                // Held across the inner block so reverse local destruction order
                // releases `child` and `mooring` before `live` on both normal
                // return and unwind.
                let result = {
                    let mooring = mooring;
                    let mut child = child;
                    f(&mooring, &mut child)
                };
                drop(live);
                result
            })?;
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::runtime::state::DEFAULT_STACK_LIMIT;
    use crate::shell::runtime::{DefaultPolicy, HookName, HookSig, block_over};

    fn inherited() -> Streams<Sink> {
        Streams {
            stdout: Sink::Terminal,
            stderr: Sink::Stderr,
        }
    }

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
            .spawn_worker(Mooring::adrift(), inherited(), |_, child| {
                (
                    child.has_hook(&HookName::session("prompt")),
                    matches!(child.io.stdin, crate::io::Source::Empty),
                )
            })
            .expect("spawn_worker");
        assert_eq!(join.join().expect("worker thread"), (false, true));
    }

    /// A worker's isolation is its birth's, whatever its parent's place: it
    /// joins no group and is not interactive, so its externals lead sessions
    /// of their own.  A stage and an aside keep the parent's interactivity.
    #[test]
    fn a_worker_is_born_isolated_and_a_stage_joins_its_group() {
        let mut parent = crate::test_helper::core_shell();
        parent.set_interactive(true);
        let group = crate::process::Pgid::from_raw(1).expect("1 is positive");
        parent.session.stage = Some(Membership::new(group, parent.scope.clone()));
        let place = |child: &Shell| (child.session.interactive, child.session.stage.is_some());

        let worker = parent
            .spawn_worker(Mooring::adrift(), inherited(), move |_, child| place(child))
            .expect("spawn_worker");
        assert_eq!(worker.join().expect("worker thread"), (false, false));

        let membership = Membership::new(group, parent.scope.clone());
        let stage = parent
            .spawn_stage(
                Mooring::adrift(),
                Io::default(),
                membership,
                move |_, child| place(child),
            )
            .expect("spawn_stage");
        assert_eq!(stage.join().expect("stage thread"), (true, true));

        assert_eq!(place(&parent.join_session()), (true, true));
    }

    /// An rc `recursion-limit:` key or a `--recursion-limit` flag configures
    /// the session as `session.stack_limit`, so a `spawn` / `par` / `watch`
    /// body must not silently fall back to the compile-time default.
    #[test]
    fn spawned_worker_inherits_the_stack_limit() {
        let mut parent = crate::test_helper::core_shell();
        parent.set_stack_limit(DEFAULT_STACK_LIMIT + 7);
        let join = parent
            .spawn_worker(Mooring::adrift(), inherited(), |_, child| {
                child.session.stack_limit
            })
            .expect("spawn_worker");

        let stack_limit = join.join().expect("worker thread");
        assert_eq!(stack_limit, DEFAULT_STACK_LIMIT + 7);
    }

    /// `session.sources` must ride into a spawned worker's shell, else a
    /// `spawn` body's error span resolves against nothing and the diagnostic
    /// falls back to caret-less rendering.
    #[test]
    fn spawned_worker_renders_errors_against_the_parents_source() {
        let mut parent = crate::test_helper::core_shell();
        let file = parent.install_script_context(
            &crate::source::SourceOrigin::File(
                crate::source::SourceFile::new("worker.ral").expect("UTF-8"),
            ),
            "one\ntwo\nbad\n",
        );
        let span = crate::source::Span::new(file, 8, 11);
        let join = parent
            .spawn_worker(Mooring::adrift(), inherited(), move |_, child| {
                crate::shell::runtime::Error {
                    prov: Some(crate::shell::runtime::Prov::Live {
                        name: "fail".into(),
                        span,
                    }),
                    ..crate::shell::runtime::Error::new("boom")
                }
                .render(&child.session.sources, None, child.exit_hints())
            })
            .expect("spawn_worker");

        let rendered = join.join().expect("worker thread");
        assert!(
            rendered.contains("worker.ral"),
            "the worker's error must resolve against the parent's source: {rendered}"
        );
    }

    /// `session.root_file` must ride into a spawned worker's shell alongside
    /// `sources`, else a bare `source`/`use` in a `spawn` body or a
    /// pipeline stage — which starts no load of its own — falls back through
    /// a missing registry entry to cwd-relative resolution instead of the
    /// run's own root, and can silently load the wrong file.
    #[test]
    fn spawned_worker_inherits_the_root_file() {
        let mut parent = crate::test_helper::core_shell();
        let root_file = Some(parent.install_script_context(
            &crate::source::SourceOrigin::File(
                crate::source::SourceFile::new("main.ral").expect("UTF-8"),
            ),
            "",
        ));
        parent.session.root_file = root_file;
        let join = parent
            .spawn_worker(Mooring::adrift(), inherited(), |_, child| {
                child.session.root_file
            })
            .expect("spawn_worker");

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
        parent.set_exit_hints(crate::shell::runtime::ExitHints::from_text(
            "false 1 hinted\n",
        ));
        let join = parent
            .spawn_worker(Mooring::adrift(), inherited(), |_, child| {
                Arc::clone(&child.session.exit_hints)
            })
            .expect("spawn_worker");

        let hints = join.join().expect("worker thread");
        assert!(Arc::ptr_eq(&hints, &parent.session.exit_hints));
    }

    /// The scratch the session's front end made for it reaches every child:
    /// a worker, a stage and an aside freeze a `scratch` place against it.
    #[test]
    fn a_child_inherits_the_scratch() {
        let mut parent = crate::test_helper::core_shell();
        parent.set_scratch("/the/session/scratch");
        let aside = parent.join_session();
        assert_eq!(aside.scratch(), Some("/the/session/scratch"));
        let join = parent
            .spawn_worker(Mooring::adrift(), inherited(), |_, child| {
                child.scratch().map(str::to_owned)
            })
            .expect("spawn_worker");
        assert_eq!(
            join.join().expect("worker thread").as_deref(),
            Some("/the/session/scratch")
        );
    }

    /// An aside is the same session: it leases nothing of its own.
    #[test]
    fn an_aside_leases_nothing() {
        let mut parent = crate::test_helper::core_shell();
        parent.arm_binding_lease(crate::shell::runtime::BindingLease {
            idle_calls: 3,
            large_binding_bytes: 1 << 20,
        });
        let aside = parent.join_session();
        assert!(!aside.local.bindings.armed());
    }

    /// An aside is the same session: it shares the fixings.
    #[test]
    fn an_aside_shares_the_session_fixings() {
        let parent = crate::test_helper::core_shell();
        let aside = parent.join_session();
        assert!(Arc::ptr_eq(&parent.session.fixings, &aside.session.fixings));
    }

    /// The live-thread ticket must outlive both `child` and `mooring` teardown
    /// on normal return and when `f` unwinds (such as a completion-delivery panic).
    #[test]
    fn live_ticket_outlives_child_and_mooring_teardown_on_return_and_unwind() {
        use crate::data::FOValue;
        use crate::shell::runtime::EventSink;
        use crate::sync::LockExt as _;
        use std::sync::Mutex;
        use std::sync::mpsc::{self, Receiver, SyncSender};
        use std::time::Duration;

        struct Gate {
            entered: SyncSender<()>,
            release: Mutex<Receiver<()>>,
        }

        impl Gate {
            fn pair() -> (Arc<Self>, Receiver<()>, SyncSender<()>) {
                let (entered_tx, entered_rx) = mpsc::sync_channel(1);
                let (release_tx, release_rx) = mpsc::sync_channel(1);
                (
                    Arc::new(Self {
                        entered: entered_tx,
                        release: Mutex::new(release_rx),
                    }),
                    entered_rx,
                    release_tx,
                )
            }

            fn wait(&self) {
                let _ = self.entered.send(());
                let _ = self.release.lock_ignore_poison().recv();
            }
        }

        struct GatedDrop(Arc<Gate>);
        impl Drop for GatedDrop {
            fn drop(&mut self) {
                self.0.wait();
            }
        }

        struct GatedSurface(Arc<Gate>);
        impl EventSink for GatedSurface {
            fn emit(&self, _: &FOValue) {}
        }
        impl Drop for GatedSurface {
            fn drop(&mut self) {
                self.0.wait();
            }
        }

        struct ReleaseOnDrop(Vec<SyncSender<()>>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                for tx in &self.0 {
                    let _ = tx.try_send(());
                }
            }
        }

        for should_panic in [false, true] {
            let parent = crate::test_helper::core_shell();
            let probe = parent.local.workers.live_ticket();
            // `parent` holds 1, `probe` holds 1 -> baseline strong_count is 2.
            let baseline = Arc::strong_count(&probe);

            let (child_gate, child_entered, child_release) = Gate::pair();
            let (mooring_gate, mooring_entered, mooring_release) = Gate::pair();
            let _guard = ReleaseOnDrop(vec![child_release.clone(), mooring_release.clone()]);

            let mut mooring = Mooring::adrift();
            mooring.surface = Some(Arc::new(GatedSurface(mooring_gate)));

            let join = parent
                .spawn_worker(mooring, inherited(), move |_, child| {
                    let on_drop = GatedDrop(child_gate);
                    child.io.stdout = Sink::Watch {
                        line: Arc::new(move |_| {
                            let _keep = &on_drop;
                        }),
                        pending: Vec::new(),
                    };
                    assert!(!should_panic, "simulated completion delivery panic");
                    42usize
                })
                .expect("spawn_worker");

            child_entered
                .recv_timeout(Duration::from_secs(2))
                .expect("child destructor must enter");
            assert!(
                Arc::strong_count(&probe) > baseline,
                "live ticket must still be held while child shell is dropping (panic={should_panic})"
            );
            child_release.send(()).expect("release child destructor");

            mooring_entered
                .recv_timeout(Duration::from_secs(2))
                .expect("mooring destructor must enter");
            assert!(
                Arc::strong_count(&probe) > baseline,
                "live ticket must still be held while mooring is dropping (panic={should_panic})"
            );
            mooring_release
                .send(())
                .expect("release mooring destructor");

            let res = join.join();
            assert_eq!(res.is_err(), should_panic);
            assert_eq!(
                Arc::strong_count(&probe),
                baseline,
                "live ticket must be released once thread exits (panic={should_panic})"
            );
        }
    }
}
