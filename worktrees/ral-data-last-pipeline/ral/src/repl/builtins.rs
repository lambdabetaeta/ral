//! REPL-only built-in commands.
//!
//! Job-control commands (`jobs`, `fg`, `bg`, `disown`) that manipulate the
//! job table directly rather than going through the evaluator.

use ral_core::diagnostic;

/// Dispatch a job-control command (`jobs`, `fg`, `bg`, `disown`).
/// Returns `true` if the input was recognised as a job command and handled.
#[cfg(unix)]
pub(super) fn handle_job_command(
    input: &str,
    jt: &mut crate::jobs::JobTable,
    stdin_tty: bool,
) -> bool {
    use crate::jobs;

    let mut parts = input.splitn(2, |c: char| c.is_whitespace());
    let Some(cmd) = parts.next() else {
        return false;
    };
    let arg = parts.next().map(str::trim);
    // Pick an explicit `n` arg if given, else the current job — shared by fg/bg/disown.
    let resolve = |arg: Option<&str>| -> Option<usize> {
        arg.and_then(|s| s.parse().ok())
            .or_else(|| jt.current().map(|j| j.id))
    };

    match cmd {
        "jobs" => {
            for job in jt.list() {
                let state = match job.state {
                    jobs::JobState::Running => "running",
                    jobs::JobState::Stopped => "stopped",
                };
                eprintln!("[{}] {} {}\t{}", job.id, state, job.pid, job.cmd);
            }
            true
        }
        "fg" => {
            let id = resolve(arg);
            match id.and_then(|id| jt.resume(id)) {
                Some(pid) => {
                    let (_, stopped) = jobs::wait_foreground(pid, stdin_tty);
                    if stopped {
                        jt.stop(pid);
                        eprintln!("[stopped]");
                    } else if let Some(id) = id {
                        jt.remove(id);
                    }
                }
                None => diagnostic::cmd_error("fg", "no such job"),
            }
            true
        }
        "bg" => {
            if resolve(arg).and_then(|id| jt.resume(id)).is_none() {
                diagnostic::cmd_error("bg", "no such job");
            }
            true
        }
        "disown" => {
            match resolve(arg) {
                Some(id) => {
                    if jt.remove(id).is_none() {
                        diagnostic::cmd_error("disown", "no such job");
                    }
                }
                None => diagnostic::cmd_error("disown", "no current job"),
            }
            true
        }
        _ => false,
    }
}
