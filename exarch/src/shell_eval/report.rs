//! The one composition of a dispatch's model-facing ending.
//!
//! [`render`] is the single stderr-composing function for a tool call: the
//! engine's own rendering, its remedy, the audit of what already stands, and
//! the orphaned-work sentence — in that order, and nowhere else.  Everything
//! it reads is handed in, so it never touches a transport or a registry
//! itself.

use super::{ToolResult, ral_value_to_text};
use ral_core::protocol::Ending;
use ral_core::protocol::probe::WorkerRow;
use std::collections::HashSet;

/// Enough of one call's fan-out to name without crowding the stderr it rides
/// on; the rest is counted aloud, never dropped in silence.
const NAMED: usize = 5;

/// The model's sections of a run that reached evaluation: its captured
/// streams, its settled value, and [`render`]'s suffix and exit; `audit` is
/// the sentence for what the run already committed, if any.
pub(crate) fn tool_result(
    ending: &Ending,
    captured: Option<ral_core::Captured>,
    births: &HashSet<u64>,
    audit: Option<&str>,
    workers: &[WorkerRow],
    timeout_secs: u64,
) -> ToolResult {
    let ral_core::Captured { stdout, mut stderr } = captured.unwrap_or_default();
    let value = match ending {
        Ending::Settled { value, .. } => ral_value_to_text(value),
        _ => None,
    };
    let (suffix, exit) = render(ending, births, audit, workers, timeout_secs);
    stderr.extend_from_slice(suffix.as_bytes());
    ToolResult {
        stdout,
        stderr,
        value,
        exit,
    }
}

/// Compose a dispatch's ending into the stderr suffix the model reads, and
/// the exit code its `EXIT:` section carries.
///
/// `births` and `workers` are read-only, boundary-legal snapshots: the
/// `Observed::Worker` ids this dispatch's drain heard, and the `` `workers `` probe
/// taken at the run boundary.  The audit and the orphan sentence never draw
/// on a [`Ending::Settled`] ending — it leaves the model well able to see
/// from the transcript what landed.
fn render(
    ending: &Ending,
    births: &HashSet<u64>,
    audit: Option<&str>,
    workers: &[WorkerRow],
    timeout_secs: u64,
) -> (String, i32) {
    let mut out = String::new();
    let exit = match ending {
        Ending::Settled { .. } => return (out, 0),
        Ending::Walled {
            rendered, status, ..
        } => {
            out.push_str(rendered);
            out.push_str(&timeout_tip(timeout_secs));
            status.get()
        }
        Ending::Raised {
            rendered,
            command_exit,
            single_command,
            status,
            ..
        } => {
            out.push_str(rendered);
            if *command_exit {
                out.push_str(&exit_tip(*single_command));
            }
            status.get()
        }
        Ending::Unreturnable { rendered, .. } => {
            out.push_str(rendered);
            ending.status()
        }
        Ending::Exited(code) => *code,
    };

    if let Some(audit) = audit {
        out.push_str(audit);
    }
    if let Some(orphans) = orphan_note(ending, births, workers) {
        out.push_str(&orphans);
    }
    (out, exit)
}

fn timeout_tip(timeout_secs: u64) -> String {
    format!(
        "\nthis call timed out after {timeout_secs}s at the point above. The steps \
         before it completed and their definitions are still bound; the step it names \
         did not complete, and the steps after it did not run: resume from there \
         rather than replaying this call.\n\
         recovery: if the command is simply slow and there is nothing to overlap it \
         with, retry with a higher `timeout_secs`. If other work can run alongside it, \
         defer it instead (`let h = defer {{ … }}`) and let the run return: the host \
         notifies you at the next exchange boundary when it settles and renders its \
         output on the rail, and `await $h` gives you its value record: you need not \
         poll.\n"
    )
}

fn exit_tip(single_command: bool) -> String {
    let mut tip = String::from(
        "\nrecovery: this non-zero exit raised. If the exit code is the tool own \
         signal rather than a failure (grep no-match=1, diff differs=1, test false=1, \
         valgrind --error-exitcode=N), its stdout/stderr were captured: read them as \
         data with `audit { … }`, which does not raise, or catch with \
         `try { … } { |err| … }`. For a yes/no check use `succeeds { … }`.",
    );
    if !single_command {
        tip.push_str(
            " A non-zero exit also aborts the rest of this command: the steps after it \
             never ran, while the definitions that completed before it are still bound: \
             resume from the failing step rather than replaying the whole call. Wrap \
             risky tools in `audit`/`try`, or split them out.",
        );
    }
    tip.push('\n');
    tip
}

/// The sentence a failed ending owes the model about work that outlived it: a
/// birth this dispatch made, still present in the registry — running or
/// settled-unclaimed — is joined against `workers` by id.  A consumed worker
/// has already left the registry and is nobody's orphan.  `None` when this
/// dispatch spawned nothing still present — silence is then the whole truth.
fn orphan_note(ending: &Ending, births: &HashSet<u64>, workers: &[WorkerRow]) -> Option<String> {
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
    let fate = match ending {
        Ending::Unreturnable { .. } => {
            "A handle this call bound with `let` is still bound: `await $h` reaches it; \
             one it only returned was lost with the result, so that work is orphaned."
        }
        _ => {
            "A handle bound by a step that completed before the failure is still bound: \
             `await $h` reaches it; one the failing step would have bound never landed, so \
             that work is orphaned."
        }
    };
    Some(format!(
        "\nwork this call spawned outlived it: {named}{overflow}. {fate}\n"
    ))
}

#[cfg(test)]
mod tests;
