//! Captured builtin entries for job-control and plugin-lifecycle commands.
//!
//! [`build`] returns the six entries installed into the session builtin
//! table at REPL boot.  The closures receive the `args` slice verbatim,
//! with no handler argv packing.

use ral_core::diagnostic;
use ral_core::typecheck::builtins::{BuiltinTypeRule, sig};
use ral_core::types::{
    Break, BuiltinBody, BuiltinEntry, HandleState, Resident, WorkerEntry,
};
use ral_core::{Shell, Value};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use super::plugin::PluginRuntime;

/// Build the six session builtin entries, capturing `jobs` and
/// `runtime` by `Arc<Mutex<…>>` so each closure owns its share of the
/// long-lived state.
pub fn build(
    jobs: Arc<Mutex<crate::jobs::JobTable>>,
    runtime: Arc<Mutex<PluginRuntime>>,
) -> Arc<[BuiltinEntry]> {
    vec![
        build_jobs(jobs.clone()),
        build_fg(jobs.clone()),
        build_bg(jobs.clone()),
        build_disown(jobs),
        build_load_plugin(runtime.clone()),
        build_unload_plugin(runtime),
    ]
    .into()
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// The plugin name a `load-plugin`/`unload-plugin` invocation targets.
/// Both verbs are `STRING_TO_UNIT`, so type-checking guarantees the one
/// argument; an empty slice can only be an internal error, handled like the
/// job verbs' "no current job" via `cmd_error` rather than a raised fault.
fn plugin_name_arg(args: &[Value]) -> Option<String> {
    args.first().map(std::string::ToString::to_string)
}

/// Resolve the job id a `fg`/`bg`/`disown` invocation targets: the explicit
/// Int argument when given, otherwise the most recent job (SPEC §18's
/// "current job" default).  `None` means there is no job to act on.
fn job_id_arg(args: &[Value], jobs: &crate::jobs::JobTable) -> Option<usize> {
    match args.first().and_then(ral_core::Value::as_int) {
        Some(n) => Some(usize::try_from(n).unwrap_or(usize::MAX)),
        None => jobs.most_recent_id(),
    }
}

/// The "no such job" elaboration for `fg`/`bg`/`disown`, which are pgid-only:
/// a worker handle has no SIGCONT, terminal, or kernel-stopped state, so an id
/// that resolves no pgid job is pointed at the handle's own eliminators rather
/// than left as a dead end for a user who tried a `[wN]` designator.
const NOT_A_PGID_JOB: &str = "no such job — fg/bg/disown are pgid-only; a worker handle's own \
     eliminators are its analogues: `await` is its fg, `cancel` its kill (see `jobs`)";

// ── jobs ─────────────────────────────────────────────────────────────────────

/// Render the `jobs` listing as one fold over both populations the session
/// backgrounds work into: `jt`'s pgid groups exactly as before, then this
/// shell's registered worker handles (`spawn`/`watch`/`&`), marked `[wN]` —
/// a designator namespace of its own so it can never collide with a pgid's
/// `[n]`. A worker renders `running (worker)` while its handle is live and
/// `done (worker)` once settled but unclaimed — the POSIX-`Done` analogue —
/// until an eliminator observes it away, at which point it is simply no
/// longer in the registry and this fold never sees it again: no separate
/// retention state lives here, and no lease is renewed by listing
/// (`Shell::workers()` already guarantees both).
fn render_jobs(jt: &crate::jobs::JobTable, workers: &[WorkerEntry]) -> Vec<String> {
    // The designator and state word come from the resident signature — the
    // one thing every population answers alike; `pgid`/`cmd` stay direct
    // field reads, the honest per-chapter variance the trait deliberately
    // leaves unflattened.
    let mut lines: Vec<String> = jt
        .list()
        .into_iter()
        .map(|job| {
            format!(
                "[{}] {} {}\t{}",
                job.designator(),
                job.state_label(),
                job.pgid,
                job.cmd
            )
        })
        .collect();
    lines.extend(workers.iter().map(|entry| {
        format!(
            "[{}] {}\t{}",
            entry.designator(),
            entry.state_label(),
            entry.cmd
        )
    }));
    lines
}

/// Compose the shell-exit survivor warning: one compact line naming every
/// worker handle still `Running` when the REPL tears down, or `None` when
/// none are — the deferred survivor warning
/// (`decisions/260616_unify-turn-evaluation`) finally landing as a fold
/// over the ledger, POSIX's "you have stopped jobs" register for the
/// population that dies with the process rather than surviving an exit
/// sweep. Called before [`crate::jobs::JobTable::cleanup`] so a worker is
/// named first, swept (with the pgid groups) second; never gates or delays
/// exit.
pub(crate) fn survivor_warning(workers: &[WorkerEntry]) -> Option<String> {
    let running: Vec<String> = workers
        .iter()
        .filter(|entry| *entry.handle.state.lock().unwrap() == HandleState::Running)
        .map(|entry| format!("[{}] {}", entry.designator(), entry.cmd))
        .collect();
    if running.is_empty() {
        return None;
    }
    Some(format!(
        "ral: {} worker{} still running and will not survive this exit: {}",
        running.len(),
        if running.len() == 1 { "" } else { "s" },
        running.join(", ")
    ))
}

fn build_jobs(jobs: Arc<Mutex<crate::jobs::JobTable>>) -> BuiltinEntry {
    BuiltinEntry {
        name: Cow::Borrowed("jobs"),
        type_rule: BuiltinTypeRule::Sig(sig::TERMINAL_CONTROL),
        doc: "jobs  — list active background and stopped jobs: pgid groups, and this shell's \
              detached worker handles (spawn/watch/&) marked [wN], done once settled until \
              observed.",
        body: BuiltinBody::Captured(Arc::new(move |_args, shell| {
            let jt = jobs.lock().unwrap();
            let workers = shell.workers();
            for line in render_jobs(&jt, &workers) {
                eprintln!("{line}");
            }
            Ok(Value::Unit)
        })),
    }
}

// ── fg ────────────────────────────────────────────────────────────────────────

fn build_fg(jobs: Arc<Mutex<crate::jobs::JobTable>>) -> BuiltinEntry {
    BuiltinEntry {
        name: Cow::Borrowed("fg"),
        type_rule: BuiltinTypeRule::Sig(sig::OPTIONAL_INT_TO_UNIT),
        doc: "fg [id]  — bring pgid job [id] (default: most recent) to the foreground. \
              pgid-only: a worker handle has no foreground — `await` is its fg.",
        body: BuiltinBody::Captured(Arc::new(move |args, shell| {
            let (id, pgid) = {
                let mut jt = jobs.lock().unwrap();
                let Some(id) = job_id_arg(args, &jt) else {
                    diagnostic::cmd_error("fg", "no current job");
                    return Ok(Value::Unit);
                };
                (id, jt.resume(id))
            };
            match pgid {
                Some(pgid) => {
                    let wait = crate::jobs::wait_foreground(pgid, shell);
                    let mut jt = jobs.lock().unwrap();
                    if wait.stopped() {
                        jt.stop(pgid);
                        eprintln!("[stopped]");
                    } else {
                        jt.remove(id);
                    }
                }
                None => diagnostic::cmd_error("fg", NOT_A_PGID_JOB),
            }
            Ok(Value::Unit)
        })),
    }
}

// ── bg ────────────────────────────────────────────────────────────────────────

fn build_bg(jobs: Arc<Mutex<crate::jobs::JobTable>>) -> BuiltinEntry {
    BuiltinEntry {
        name: Cow::Borrowed("bg"),
        type_rule: BuiltinTypeRule::Sig(sig::OPTIONAL_INT_TO_UNIT),
        doc: "bg [id]  — resume pgid job [id] (default: most recent) in the background. \
              pgid-only: a worker handle already runs detached — see `jobs`.",
        body: BuiltinBody::Captured(Arc::new(move |args, _shell| {
            let resumed = {
                let mut jt = jobs.lock().unwrap();
                let Some(id) = job_id_arg(args, &jt) else {
                    diagnostic::cmd_error("bg", "no current job");
                    return Ok(Value::Unit);
                };
                jt.resume_in_background(id)
            };
            if resumed.is_none() {
                diagnostic::cmd_error("bg", NOT_A_PGID_JOB);
            }
            Ok(Value::Unit)
        })),
    }
}

// ── disown ───────────────────────────────────────────────────────────────────

fn build_disown(jobs: Arc<Mutex<crate::jobs::JobTable>>) -> BuiltinEntry {
    BuiltinEntry {
        name: Cow::Borrowed("disown"),
        type_rule: BuiltinTypeRule::Sig(sig::OPTIONAL_INT_TO_UNIT),
        doc: "disown [id]  — detach pgid job [id] (default: most recent) from the shell. \
              pgid-only: a worker handle has no disown — `cancel` is its kill.",
        body: BuiltinBody::Captured(Arc::new(move |args, _shell| {
            let removed = {
                let mut jt = jobs.lock().unwrap();
                let Some(id) = job_id_arg(args, &jt) else {
                    diagnostic::cmd_error("disown", "no current job");
                    return Ok(Value::Unit);
                };
                jt.remove(id)
            };
            match removed {
                Some(_job) => {
                    #[cfg(windows)]
                    ral_core::process::disown_pipeline_group(ral_core::process::Pgid(_job.pgid));
                }
                None => diagnostic::cmd_error("disown", NOT_A_PGID_JOB),
            }
            Ok(Value::Unit)
        })),
    }
}

// ── load-plugin ───────────────────────────────────────────────────────────────

fn build_load_plugin(runtime: Arc<Mutex<PluginRuntime>>) -> BuiltinEntry {
    BuiltinEntry {
        name: Cow::Borrowed("load-plugin"),
        type_rule: BuiltinTypeRule::Sig(sig::STRING_TO_UNIT),
        doc: "load-plugin <name>  — load a REPL plugin by name or path.",
        body: BuiltinBody::Captured(Arc::new(move |args, shell: &mut Shell| {
            let Some(name) = plugin_name_arg(args) else {
                diagnostic::cmd_error("load-plugin", "missing plugin name");
                return Ok(Value::Unit);
            };
            if let Err(Break::Error(e)) =
                super::plugin::load::load_plugin(&name, None, shell, &runtime)
            {
                diagnostic::cmd_error("load-plugin", &e.message);
            }
            Ok(Value::Unit)
        })),
    }
}

// ── unload-plugin ─────────────────────────────────────────────────────────────

fn build_unload_plugin(runtime: Arc<Mutex<PluginRuntime>>) -> BuiltinEntry {
    BuiltinEntry {
        name: Cow::Borrowed("unload-plugin"),
        type_rule: BuiltinTypeRule::Sig(sig::STRING_TO_UNIT),
        doc: "unload-plugin <name>  — unload a previously loaded REPL plugin.",
        body: BuiltinBody::Captured(Arc::new(move |args, shell: &mut Shell| {
            let Some(name) = plugin_name_arg(args) else {
                diagnostic::cmd_error("unload-plugin", "missing plugin name");
                return Ok(Value::Unit);
            };
            if let Err(e) = super::plugin::load::unload_plugin(&name, shell, &runtime) {
                diagnostic::cmd_error("unload-plugin", &e.message);
            }
            Ok(Value::Unit)
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{JobState, JobTable};

    /// J7: a bare `fg`/`bg`/`disown` defaults to the most recent job; an
    /// explicit id wins; an empty table yields no job.
    #[test]
    fn job_id_arg_defaults_to_most_recent() {
        let mut jt = JobTable::new();

        // No arg, empty table → no current job.
        assert_eq!(job_id_arg(&[], &jt), None);

        jt.add(1001, "first".into(), JobState::Running);
        jt.add(1002, "second".into(), JobState::Stopped);

        // No arg → the most recent (highest id) job.
        assert_eq!(job_id_arg(&[], &jt), Some(2));

        // Explicit id wins over the default.
        assert_eq!(job_id_arg(&[Value::Int(1)], &jt), Some(1));
    }

    /// A minimal registered-worker fixture, `running` toggling
    /// [`HandleState::Running`] vs [`HandleState::Completed`] — enough to
    /// exercise [`render_jobs`] and [`survivor_warning`] without a real
    /// `spawn`.  Every `HandleInner` field is legitimately public
    /// (`decisions/260615_no-core-repr-leak-into-exarch` draws that line at
    /// exarch, not at a sibling crate reading core's own types), the same
    /// construction core's own concurrency tests use.
    fn fake_worker(id: u64, cmd: &str, running: bool) -> WorkerEntry {
        let state = if running {
            HandleState::Running
        } else {
            HandleState::Completed
        };
        WorkerEntry {
            id: ral_core::types::WorkerId(id),
            cmd: cmd.to_string(),
            started: std::time::SystemTime::now(),
            class: ral_core::types::LeaseClass::Worker,
            settled_epoch: None,
            handle: ral_core::types::HandleInner {
                result: Arc::new(Mutex::new(None)),
                cached: Arc::new(Mutex::new(None)),
                state: Arc::new(Mutex::new(state)),
                stdout_buf: Arc::new(Mutex::new(Vec::new())),
                stderr_buf: Arc::new(Mutex::new(Vec::new())),
                surface_buf: Arc::new(Mutex::new(Vec::new())),
                joined: Arc::new(Mutex::new(false)),
                last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
                cmd: cmd.to_string(),
                cancel: ral_core::process::CancelScope::default(),
            },
        }
    }

    /// `render_jobs` lists pgid jobs first, then `[wN]`-marked worker handles
    /// (`running`/`done (worker)`), and an observed-away worker never reappears.
    #[test]
    fn render_jobs_folds_pgid_and_worker_populations() {
        let mut jt = JobTable::new();
        jt.add(1001, "vim".into(), JobState::Stopped);
        let workers = vec![
            fake_worker(3, "spawn { long_task }", true),
            fake_worker(7, "watch { tail }", false),
        ];

        let lines = render_jobs(&jt, &workers);
        assert_eq!(lines.len(), 3, "one pgid job plus two worker handles");
        assert!(lines[0].starts_with("[1] stopped 1001\tvim"));
        assert_eq!(lines[1], "[w3] running (worker)\tspawn { long_task }");
        assert_eq!(lines[2], "[w7] done (worker)\twatch { tail }");

        // Once observed away, a worker is simply absent from the next
        // snapshot — no separate "done" bookkeeping survives it here.
        let lines_after_observe = render_jobs(&jt, &[fake_worker(3, "spawn { long_task }", true)]);
        assert!(
            !lines_after_observe.iter().any(|l| l.contains("[w7]")),
            "an observed-away worker never renders again"
        );
    }

    /// `survivor_warning` names every still-running worker in one line and is
    /// `None` when the registry holds none.
    #[test]
    fn survivor_warning_names_running_workers_only() {
        assert_eq!(
            survivor_warning(&[]),
            None,
            "nothing running, nothing to warn"
        );

        let settled_only = vec![fake_worker(1, "spawn { done }", false)];
        assert_eq!(
            survivor_warning(&settled_only),
            None,
            "a settled-but-unclaimed worker is not a survivor"
        );

        let mixed = vec![
            fake_worker(2, "spawn { still_going }", true),
            fake_worker(9, "service { daemon }", true),
            fake_worker(1, "spawn { done }", false),
        ];
        let warning = survivor_warning(&mixed).expect("two running workers must be named");
        assert!(warning.contains("2 workers"), "got: {warning}");
        assert!(
            warning.contains("[w2] spawn { still_going }"),
            "got: {warning}"
        );
        assert!(
            warning.contains("[w9] service { daemon }"),
            "got: {warning}"
        );
        assert!(!warning.contains("[w1]"), "the settled worker is not named");
    }

    /// `fg`/`bg`/`disown` are strictly pgid-typed: an id that resolves no
    /// pgid job is met with the correspondence to a worker handle's own
    /// eliminators, never a bare "no such job" that strands a `[wN]` user.
    #[test]
    fn not_a_pgid_job_names_the_handle_correspondence() {
        assert!(NOT_A_PGID_JOB.contains("pgid-only"));
        assert!(NOT_A_PGID_JOB.contains("`await`"), "fg's analogue");
        assert!(NOT_A_PGID_JOB.contains("`cancel`"), "the kill analogue");
    }

    /// Vet refusal: `alias jobs …` on a REPL-dressed table is rejected, not
    /// silently installed to shadow `jobs` at dispatch.  [`build`] yields
    /// the six captured entries `Session::boot`'s surface carries
    /// (`repl/session.rs`), so `jobs` sits on the table exactly as it would
    /// in a booted REPL session; `install_alias` reads that same table via
    /// `HandlerEntry::vet`.
    #[test]
    fn alias_over_a_captured_builtin_is_rejected() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        shell.install_captured_builtins(build(
            Arc::new(Mutex::new(crate::jobs::JobTable::new())),
            Arc::new(Mutex::new(PluginRuntime::default())),
        ));

        let ast = ral_core::syntax::parser::parse("{ |args| return 1 }").unwrap();
        let comp = std::sync::Arc::new(ral_core::elaborator::elaborate(
            &ast,
            std::collections::HashSet::default(),
        ));
        let thunk = ral_core::evaluator::evaluate(&comp, &mut shell).unwrap();

        let err = shell
            .install_alias("jobs".to_string(), thunk)
            .expect_err("aliasing a captured builtin must be refused");
        let ral_core::types::Break::Error(e) = err else {
            panic!("expected a catchable Error, got an Escape: {err:?}");
        };
        assert!(
            e.message.contains("builtin"),
            "expected the vet's builtin-collision message, got: {}",
            e.message
        );
    }
}
