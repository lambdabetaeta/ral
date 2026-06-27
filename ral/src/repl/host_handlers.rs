//! Captured builtin entries for job-control and plugin-lifecycle commands.
//!
//! [`build`] returns the six entries installed into the session builtin
//! table at REPL boot.  The closures receive the `args` slice verbatim,
//! with no handler argv packing.

use ral_core::diagnostic;
use ral_core::typecheck::builtins::{BuiltinTypeRule, sig};
use ral_core::types::{Break, BuiltinBody, BuiltinEntry, Error};
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
        build_disown(jobs.clone()),
        build_load_plugin(runtime.clone()),
        build_unload_plugin(runtime.clone()),
    ]
    .into()
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn type_err(msg: &'static str) -> Break {
    Break::Error(Error::new(msg, 1))
}

/// Resolve the job id a `fg`/`bg`/`disown` invocation targets: the explicit
/// Int argument when given, otherwise the most recent job (SPEC §18's
/// "current job" default).  `None` means there is no job to act on.
fn job_id_arg(args: &[Value], jobs: &crate::jobs::JobTable) -> Option<usize> {
    match args.first().and_then(|v| v.as_int()) {
        Some(n) => Some(n as usize),
        None => jobs.most_recent_id(),
    }
}

// ── jobs ─────────────────────────────────────────────────────────────────────

fn build_jobs(jobs: Arc<Mutex<crate::jobs::JobTable>>) -> BuiltinEntry {
    BuiltinEntry {
        name: Cow::Borrowed("jobs"),
        type_rule: BuiltinTypeRule::Sig(sig::TERMINAL_CONTROL),
        doc: "jobs  — list active background and stopped jobs.",
        body: BuiltinBody::Captured(Arc::new(move |_args, _shell| {
            let jt = jobs.lock().unwrap();
            for job in jt.list() {
                let state = match job.state {
                    crate::jobs::JobState::Running => "running",
                    crate::jobs::JobState::Stopped => "stopped",
                };
                eprintln!("[{}] {} {}\t{}", job.id, state, job.pgid, job.cmd);
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
        doc: "fg [id]  — bring job [id] (default: most recent) to the foreground.",
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
                None => diagnostic::cmd_error("fg", "no such job"),
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
        doc: "bg [id]  — resume job [id] (default: most recent) in the background.",
        body: BuiltinBody::Captured(Arc::new(move |args, _shell| {
            let mut jt = jobs.lock().unwrap();
            let Some(id) = job_id_arg(args, &jt) else {
                diagnostic::cmd_error("bg", "no current job");
                return Ok(Value::Unit);
            };
            if jt.resume_in_background(id).is_none() {
                diagnostic::cmd_error("bg", "no such job");
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
        doc: "disown [id]  — detach job [id] (default: most recent) from the shell.",
        body: BuiltinBody::Captured(Arc::new(move |args, _shell| {
            let mut jt = jobs.lock().unwrap();
            let Some(id) = job_id_arg(args, &jt) else {
                diagnostic::cmd_error("disown", "no current job");
                return Ok(Value::Unit);
            };
            match jt.remove(id) {
                Some(_job) => {
                    #[cfg(windows)]
                    ral_core::process::disown_pipeline_group(ral_core::process::Pgid(_job.pgid));
                }
                None => diagnostic::cmd_error("disown", "no such job"),
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
            let name = match args.first() {
                Some(v) => v.to_string(),
                None => return Err(type_err("load-plugin: missing plugin name")),
            };
            let name = name.trim_matches('\'').trim_matches('"').to_string();
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
            let name = match args.first() {
                Some(v) => v.to_string(),
                None => return Err(type_err("unload-plugin: missing plugin name")),
            };
            let name = name.trim_matches('\'').trim_matches('"').to_string();
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
}
