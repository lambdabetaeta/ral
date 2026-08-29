//! The one composition of a dispatch's model-facing ending.
//!
//! [`render`] is the single stderr-composing function for a tool call: the
//! engine's own rendering, its remedy, the audit of what already stands, and
//! the orphaned-work sentence — in that order, and nowhere else.  Everything
//! it reads is handed in, so it never touches a transport or a registry
//! itself.

use crate::agent::ProbedWorker;
use crate::fleet::desk::ActFragment;
use ral_core::Value as RalValue;
use ral_core::serial::FOValue;
use ral_core::protocol::Ending;
use ral_core::types::{Observation, Observed};
use std::collections::HashSet;

/// Enough of one call's fan-out to name without crowding the stderr it rides
/// on; the rest is counted aloud, never dropped in silence.
const NAMED: usize = 5;

/// Compose a dispatch's ending into the stderr suffix the model reads, and
/// the exit code its `EXIT:` section carries.
///
/// `trail` and `workers` are read-only, boundary-legal snapshots: the
/// dispatch's own [`Observed::Worker`] births, and the `` `workers `` probe
/// taken at the run boundary.  The audit and the orphan sentence never draw
/// on a [`Ending::Settled`] or [`Ending::Stopped`] ending — job control keeps
/// its bindings, and a returning call has nothing to answer for.
pub(crate) fn render(
    ending: &Ending,
    trail: &[FOValue],
    fragment: &ActFragment,
    workers: &[ProbedWorker],
    timeout_secs: u64,
) -> (String, i32) {
    let mut out = String::new();
    let exit = match ending {
        Ending::Settled { .. } => return (out, 0),
        Ending::Stopped { .. } => return (out, 1),
        Ending::Walled { rendered, .. } => {
            out.push_str(rendered);
            out.push_str(&timeout_tip(timeout_secs));
            // 124 names the wall, not the cancel's own exit status — the
            // wire carries that status for other seats ($? in the REPL),
            // but a timed-out tool call reports the timeout itself.
            124
        }
        Ending::Raised {
            rendered,
            command_exit,
            single_command,
            status,
        } => {
            out.push_str(rendered);
            if *command_exit {
                out.push_str(&exit_tip(*single_command));
            }
            *status
        }
        Ending::Exited(code) => *code,
    };

    if let Some(audit) = fragment.audit() {
        out.push_str(&audit);
    }
    if let Some(orphans) = orphan_note(trail, workers) {
        out.push_str(&orphans);
    }
    (out, exit)
}

fn timeout_tip(timeout_secs: u64) -> String {
    format!(
        "\nthis call timed out after {timeout_secs}s at the point above. The steps \
         before it completed; the step it names did not, and the bindings this call \
         made are gone.\n\
         recovery: if the command is simply slow and there is nothing to overlap it \
         with, retry with a higher `timeout_secs`. If other work can run alongside it, \
         defer it instead (`let h = defer {{ … }}`) and let the run return: the host \
         notifies you at the next exchange boundary when it settles and renders its \
         output on the rail, and `await $h` gives you its value record — you need not \
         poll.\n"
    )
}

fn exit_tip(single_command: bool) -> String {
    let mut tip = String::from(
        "\nrecovery: this non-zero exit raised. If the exit code is the tool own \
         signal rather than a failure (grep no-match=1, diff differs=1, test false=1, \
         valgrind --error-exitcode=N), its stdout/stderr were captured — read them as \
         data with `audit { … }`, which does not raise, or catch with \
         `try { … } { |err| … }`. For a yes/no check use `succeeds { … }`.",
    );
    if !single_command {
        tip.push_str(
            " A non-zero exit also aborts the rest of this command and discards earlier \
             bindings; wrap risky tools in `audit`/`try`, or split them out.",
        );
    }
    tip.push('\n');
    tip
}

/// The [`WorkerId`](ral_core::types::WorkerId)s this dispatch's own trail gave
/// birth to.
fn trail_worker_ids(trail: &[FOValue]) -> HashSet<u64> {
    trail
        .iter()
        .filter_map(|fov| Observation::from_value(&RalValue::from(fov.clone())))
        .filter_map(|obs| match obs.what {
            Observed::Worker { id, .. } => Some(id.0),
            _ => None,
        })
        .collect()
}

/// The sentence a binding-discarding ending owes the model about work that
/// outlived it: a birth this dispatch made, still present in the registry —
/// running or settled-unclaimed, both equally unreachable once the handle
/// binding unwound — is joined against `workers` by id.  A consumed worker
/// has already left the registry and is nobody's orphan.  `None` when this
/// dispatch spawned nothing still present — silence is then the whole truth.
fn orphan_note(trail: &[FOValue], workers: &[ProbedWorker]) -> Option<String> {
    let births = trail_worker_ids(trail);
    let mut cmds: Vec<String> = workers
        .iter()
        .filter(|w| births.contains(&w.id))
        .map(|w| format!("`{}`", w.cmd))
        .collect();
    if cmds.is_empty() {
        return None;
    }
    let unnamed = cmds.len().saturating_sub(NAMED);
    cmds.truncate(NAMED);
    let named = cmds.join(", ");
    let overflow = match unnamed {
        0 => String::new(),
        n => format!(", and {n} more not named here"),
    };
    Some(format!(
        "\nwork this call spawned is now orphaned: {named}{overflow}. The binding that \
         named it went with the unwind, so you cannot `await` it.\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::types::{CallSite, LeaseClass, WorkerId};

    fn worker_birth(id: u64, cmd: &str) -> FOValue {
        let obs = Observation::instant(
            CallSite::default(),
            Some("test".into()),
            Observed::Worker {
                id: WorkerId(id),
                cmd: cmd.into(),
                class: LeaseClass::Worker,
            },
        );
        FOValue::try_from(&obs.to_wire()).expect("Observation::to_wire is total")
    }

    fn worker_row(id: u64, cmd: &str, running: bool) -> ProbedWorker {
        ProbedWorker {
            id,
            cmd: cmd.to_string(),
            class: LeaseClass::Worker,
            running,
            up_secs: 0,
            idle_secs: 0,
            settled_epoch: (!running).then_some(0),
        }
    }

    fn committed_act(verb: &str, subject: Option<&str>) -> Observation {
        Observation::instant(
            CallSite::default(),
            Some("test".into()),
            Observed::Act {
                verb: verb.into(),
                subject: subject.map(str::to_string),
                payload: String::new(),
                refused: false,
            },
        )
    }

    fn settled_ending() -> Ending {
        Ending::Settled {
            value: FOValue::Unit,
            status: 0,
        }
    }

    #[cfg(unix)]
    fn stopped_ending() -> Ending {
        Ending::Stopped {
            pgid: 1,
            signal: 20,
            signal_name: "SIGTSTP".into(),
            pending: Vec::new(),
        }
    }

    #[test]
    fn a_returning_call_says_nothing() {
        let (out, exit) = render(&settled_ending(), &[], &ActFragment::default(), &[], 30);
        assert!(out.is_empty(), "a settled ending composes nothing: {out:?}");
        assert_eq!(exit, 0);
    }

    #[cfg(unix)]
    #[test]
    fn stopped_draws_neither_audit_nor_orphan() {
        let trail = vec![worker_birth(1, "sleep 20")];
        let fragment = ActFragment::from_acts(vec![committed_act("reply", None)]);
        let workers = vec![worker_row(1, "sleep 20", true)];
        let (out, exit) = render(&stopped_ending(), &trail, &fragment, &workers, 5);
        assert!(out.is_empty(), "job control keeps its bindings: {out:?}");
        assert_eq!(exit, 1);
    }

    #[test]
    fn wall_composes_rendering_remedy_audit_and_orphan_in_order() {
        let ending = Ending::Walled {
            rendered: "error: sleep 30\n".into(),
            status: 143,
        };
        let trail = vec![worker_birth(1, "sleep 20")];
        let fragment = ActFragment::from_acts(vec![committed_act("reply", None)]);
        let workers = vec![worker_row(1, "sleep 20", true)];
        let (out, exit) = render(&ending, &trail, &fragment, &workers, 5);

        assert_eq!(exit, 124);
        let rendered_at = out.find("error: sleep 30").expect("engine rendering");
        let remedy_at = out.find("recovery:").expect("timeout remedy");
        let audit_at = out.find("audit:").expect("audit sentence");
        let orphan_at = out.find("orphaned").expect("orphan sentence");
        assert!(
            rendered_at < remedy_at && remedy_at < audit_at && audit_at < orphan_at,
            "composition order must be rendering, remedy, audit, orphan: {out:?}"
        );
    }

    #[test]
    fn raise_without_command_exit_carries_no_remedy() {
        let ending = Ending::Raised {
            rendered: "error: boom\n".into(),
            command_exit: false,
            single_command: true,
            status: 7,
        };
        let (out, exit) = render(&ending, &[], &ActFragment::default(), &[], 5);
        assert_eq!(exit, 7);
        assert!(out.contains("error: boom"));
        assert!(
            !out.contains("recovery:"),
            "a raise is not a command exit: {out:?}"
        );
    }

    #[test]
    fn exit_ending_widens_to_name_a_live_orphan() {
        let ending = Ending::Exited(1);
        let trail = vec![worker_birth(9, "spawn body")];
        let workers = vec![worker_row(9, "spawn body", true)];
        let (out, exit) = render(&ending, &trail, &ActFragment::default(), &workers, 5);
        assert_eq!(exit, 1);
        assert!(
            out.contains("`spawn body`"),
            "a non-zero exit names a live orphan too, not just the wall: {out:?}"
        );
    }

    #[test]
    fn a_consumed_worker_is_nobodys_orphan() {
        let ending = Ending::Exited(1);
        let trail = vec![worker_birth(9, "spawn body")];
        let (out, _) = render(&ending, &trail, &ActFragment::default(), &[], 5);
        assert!(
            !out.contains("orphaned"),
            "absent from the probe means already consumed: {out:?}"
        );
    }

    #[test]
    fn overflow_past_named_is_counted_not_dropped() {
        let trail: Vec<FOValue> = (0..NAMED as u64 + 2)
            .map(|id| worker_birth(id, "job"))
            .collect();
        let workers: Vec<ProbedWorker> = (0..NAMED as u64 + 2)
            .map(|id| worker_row(id, "job", true))
            .collect();
        let (out, _) = render(
            &Ending::Exited(1),
            &trail,
            &ActFragment::default(),
            &workers,
            5,
        );
        assert!(out.contains("2 more not named here"), "{out:?}");
    }

    /// The full ending matrix: audit and orphan draw only on a
    /// binding-discarding ending, and only when there is something to say.
    #[cfg(unix)]
    #[test]
    fn ending_matrix_gates_audit_and_orphan_on_binding_loss() {
        let births = vec![worker_birth(3, "job")];
        let live = vec![worker_row(3, "job", true)];
        let committed = ActFragment::from_acts(vec![committed_act("spawn", Some("helper"))]);
        let refused = ActFragment::default();

        let endings: [(&str, Ending, i32); 5] = [
            ("ok", settled_ending(), 0),
            (
                "raise",
                Ending::Raised {
                    rendered: "error: boom\n".into(),
                    command_exit: false,
                    single_command: true,
                    status: 7,
                },
                7,
            ),
            (
                "wall",
                Ending::Walled {
                    rendered: "error: wall\n".into(),
                    status: 143,
                },
                124,
            ),
            ("exit", Ending::Exited(3), 3),
            ("stopped", stopped_ending(), 1),
        ];

        for (name, ending, want_exit) in &endings {
            let binding_loss = !matches!(ending, Ending::Settled { .. } | Ending::Stopped { .. });
            for (births_label, trail) in [("present", births.clone()), ("absent", Vec::new())] {
                for (acts_label, fragment) in [("committed", &committed), ("refused", &refused)] {
                    let (out, exit) = render(ending, &trail, fragment, &live, 5);
                    assert_eq!(exit, *want_exit, "{name}/{births_label}/{acts_label}");
                    assert_eq!(
                        out.contains("audit:"),
                        binding_loss && acts_label == "committed",
                        "{name}/{births_label}/{acts_label} audit mismatch: {out:?}"
                    );
                    assert_eq!(
                        out.contains("orphaned"),
                        binding_loss && births_label == "present",
                        "{name}/{births_label}/{acts_label} orphan mismatch: {out:?}"
                    );
                }
            }
        }
    }
}
