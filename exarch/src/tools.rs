//! Tool registry — one entry per tool the model may call.
//!
//! A [`Tool`] knows how to advertise itself to the provider (name,
//! description, JSON schema) and how to dispatch one parsed JSON input
//! against a live [`Agent`], returning a [`SessionToolResult`].  Every tool
//! returns synchronously: a tool that forks a child session (`agraphos` or
//! `anamnesis`) launches
//! a detached peer and returns a start receipt, so dispatch never blocks and
//! there is no join phase.
//!
//! Each tool owns its own input parsing and invalid-input UX —
//! [`Tool::dispatch`] is given a raw `serde_json::Value`, and the tool
//! decides how to read it, what label to show on the rail, and how to
//! report a malformed call.  Adding a tool means writing a sibling
//! module under `tools/` and listing it in [`registry`]; nothing in
//! `provider.rs` or `session.rs` needs to know its shape.

use crate::agent::Agent;
use crate::bus::{Emitter, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use serde_json::Value;
use std::sync::{Arc, OnceLock};

mod agent;
mod ral;
mod reply;
mod schedule;

/// The axis that gates a tool out of an agent's [`tools_for`] view.  Exactly
/// one per tool, and mutually exclusive — a property a pair of independent
/// booleans could not express, since no tool both returns and schedules.
///
/// Membership *is* the gate: a tool absent from an agent's slice is neither
/// advertised to the provider nor dispatchable, so there is no separate
/// permission predicate to keep in sync.
pub(crate) enum Gate {
    /// Unconditionally present: `ral`, the spawn family, `fff`.
    Always,
    /// Present only on a **returning** agent — `reply`, the way an agent hands
    /// a value back.  Withheld from the interactive trunk, which converses with
    /// the user across turns and never returns, so returning is meaningless
    /// there.  Spawning is universal ([[decisions/260624_uniform-agent-nodes]]),
    /// so this is the only axis separating the trunk from its sub-agents.
    Returns,
    /// Present only under the `--allow-schedule` grant — the self-wakeup family
    /// (`schedule`, `schedules`, `unschedule`).  An agent that can wake itself
    /// indefinitely holds real authority, so the grant is off by default and
    /// inherited by a fork from its parent.
    Schedules,
}

/// One registered tool.  The registry stores `Box<dyn Tool>` and
/// dispatches by name; [`Agent`] holds no tool-specific knowledge.
pub(crate) trait Tool: Send + Sync {
    /// Stable identifier used by the model and the wire schema.
    fn name(&self) -> &'static str;

    /// One-paragraph description handed to the provider.
    fn desc(&self) -> &'static str;

    /// JSON schema for the tool's input object.  Built once and cached
    /// inside the impl; cheap to call.
    fn schema(&self) -> &'static Value;

    /// Which axis gates this tool out of an agent's view ([`tools_for`]);
    /// [`Gate::Always`] for the unconditional majority.  Overridden by `reply`
    /// ([`Gate::Returns`]) and the self-wakeup family ([`Gate::Schedules`]).
    fn gate(&self) -> Gate {
        Gate::Always
    }

    /// Read `input`, render the rail header, run the call, and return its
    /// result.  A tool that forks a child session (`agraphos`/`anamnesis`) launches a
    /// detached worker and returns a start receipt here — it does not block.
    /// Malformed input is reported by the tool itself via [`invalid_input`]
    /// (or its own equivalent), so this method always produces a result.  A
    /// tool reads the live cancellation token from `session` if it needs it.
    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Agent,
        provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult;
}

/// Every tool, in the order they were added — the one static backing store the
/// per-agent views ([`tools_for`]) point into.  Built once.
fn registry() -> &'static [Box<dyn Tool>] {
    static R: OnceLock<Vec<Box<dyn Tool>>> = OnceLock::new();
    R.get_or_init(|| {
        vec![
            Box::new(ral::RalTool),
            Box::new(agent::SpawnTool::agraphos()),
            Box::new(agent::SpawnTool::anamnesis()),
            Box::new(agent::AgentsTool),
            Box::new(agent::MessageTool),
            Box::new(agent::AgentCancelTool),
            Box::new(schedule::ScheduleTool),
            Box::new(schedule::SchedulesTool),
            Box::new(schedule::UnscheduleTool),
            Box::new(reply::ReplyTool),
        ]
    })
}

/// The tools an agent may call — a per-agent view into the static [`registry`],
/// shaped by the two gate axes.  `returns` admits `reply`; `schedules` admits
/// the self-wakeup family.  The result is the agent's single source of truth:
/// it is both advertised to the provider and searched on dispatch, so a tool
/// absent here is invisible and uncallable, with no separate predicate.
pub(crate) fn tools_for(returns: bool, schedules: bool) -> Vec<&'static dyn Tool> {
    registry()
        .iter()
        .map(|b| b.as_ref())
        .filter(|t| match t.gate() {
            Gate::Always => true,
            Gate::Returns => returns,
            Gate::Schedules => schedules,
        })
        .collect()
}

/// The placeholder a malformed call passes for `display` when the JSON did
/// not even parse into args — a cross-tool sentinel.  The frontend reads it
/// to route such a call to an invisible boundary rather than render a
/// stand-in token: there is nothing meaningful to show, only the error the
/// model receives through the result body.  A call that *did* recover a real
/// offending value (a bad cron string, say) passes that value instead, and
/// it renders.
pub(crate) const INVALID_INPUT: &str = "<invalid input>";

/// Render the rail header and an error block for a malformed tool
/// call, returning the [`SessionToolResult`] the dispatcher commits.
/// `display` is the partial label the rail should show — typically
/// the field that did parse, or [`INVALID_INPUT`] when nothing did.
pub(super) fn invalid_input(
    id: String,
    tool: &'static str,
    display: &str,
    reason: &str,
    emit: &Emitter,
) -> SessionToolResult {
    emit.emit(Kind::ToolCall {
        tool,
        cmd: display.to_string(),
        summary: None,
    });
    let msg = format!("tool input error: {reason}\nexpected an object matching the tool's schema");
    emit.emit(Kind::ToolResult(msg.clone()));
    SessionToolResult { id, content: msg }
}

/// Pull a required `u64` field out of a tool's JSON input object.
pub(super) fn u64_field(input: &Value, field: &str) -> Option<u64> {
    input
        .as_object()
        .and_then(|o| o.get(field))
        .and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two gate axes select tools independently: `reply` rides `returns`,
    /// the self-wakeup family rides `schedules`, and the rest are unconditional.
    #[test]
    fn tools_for_gates_reply_and_the_wakeup_family() {
        let names = |returns, schedules| {
            tools_for(returns, schedules)
                .iter()
                .map(|t| t.name())
                .collect::<Vec<_>>()
        };

        let granted = names(true, true);
        assert!(granted.contains(&"reply"), "a returning view holds `reply`");
        for f in ["schedule", "schedules", "unschedule"] {
            assert!(granted.contains(&f), "a scheduling view holds `{f}`");
        }
        assert!(granted.contains(&"ral"), "the always-tools are present");

        let withheld = names(false, false);
        assert!(
            !withheld.contains(&"reply"),
            "the conversing view withholds `reply`"
        );
        for f in ["schedule", "schedules", "unschedule"] {
            assert!(
                !withheld.contains(&f),
                "an ungranted view withholds the wakeup tool `{f}`"
            );
        }
        assert!(
            withheld.contains(&"ral"),
            "the always-tools survive both axes off"
        );
    }
}
