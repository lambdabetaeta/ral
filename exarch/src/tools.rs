//! Tool registry — one entry per tool the model may call.
//!
//! A [`Tool`] knows how to advertise itself to the provider (name,
//! description, JSON schema) and how to dispatch one parsed JSON input
//! against a live [`Agent`], returning a [`SessionToolResult`].  Every tool
//! returns synchronously: a tool that forks a child session (`amnemon` or
//! `mnemon`) launches
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
mod commitment;
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
    /// Unconditionally present: `ral`, `agents`/`message`/`agent_cancel`, `fff`.
    Always,
    /// Present only on a **returning** agent — `reply`, the way an agent hands
    /// a value back.  Withheld from the interactive trunk, which converses with
    /// the user across turns and never returns, so returning is meaningless
    /// there.  Spawning is uniform across every agent
    /// ([[decisions/260624_uniform-agent-nodes]]) but not unconditional — see
    /// [`Spawns`](Self::Spawns) — so this and `Spawns` are the two axes
    /// separating the trunk from its sub-agents.
    Returns,
    /// Present only under the `--allow-schedule` grant — the self-wakeup family
    /// (`schedule`, `schedules`, `unschedule`).  An agent that can wake itself
    /// indefinitely holds real authority, so the grant is off by default and
    /// inherited by a fork from its parent.
    Schedules,
    /// Present only while the agent's spawn fuel is nonzero — `amnemon` /
    /// `mnemon`.  Every agent may spawn, but each fork spends one unit of
    /// the parent's fuel on the child, so a delegation chain's tools vanish a
    /// fixed number of generations down rather than recursing forever
    /// ([[decisions/260703_spawn-fuel-ceiling]]).
    Spawns,
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
    /// ([`Gate::Returns`]), the self-wakeup family ([`Gate::Schedules`]), and
    /// the spawn family ([`Gate::Spawns`]).
    fn gate(&self) -> Gate {
        Gate::Always
    }

    /// Read `input`, render the rail header, run the call, and return its
    /// result.  A tool that forks a child session (`amnemon`/`mnemon`) launches a
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
            Box::new(commitment::CommitTool),
            Box::new(commitment::VerifyCommitmentTool),
            Box::new(agent::SpawnTool::amnemon()),
            Box::new(agent::SpawnTool::mnemon()),
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
/// shaped by the three gate axes.  `returns` admits `reply`; `schedules` admits
/// the self-wakeup family; `can_spawn` admits `amnemon`/`mnemon`.  The
/// result is the agent's single source of truth: it is both advertised to the
/// provider and searched on dispatch, so a tool absent here is invisible and
/// uninvocable, with no separate predicate.
pub(crate) fn tools_for(returns: bool, schedules: bool, can_spawn: bool) -> Vec<&'static dyn Tool> {
    registry()
        .iter()
        .map(std::convert::AsRef::as_ref)
        .filter(|t| match t.gate() {
            Gate::Always => true,
            Gate::Returns => returns,
            Gate::Schedules => schedules,
            Gate::Spawns => can_spawn,
        })
        .collect()
}

pub(crate) use agent::spawn_branch;
pub(crate) use agent::spawn_discussion;
pub(crate) use commitment::CommitmentSettle;

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

    /// The three gate axes select tools independently: `reply` rides
    /// `returns`, the self-wakeup family rides `schedules`, the spawn family
    /// rides `can_spawn`, and the rest are unconditional.
    #[test]
    fn tools_for_gates_reply_wakeup_and_spawn_families() {
        let names = |returns, schedules, can_spawn| {
            tools_for(returns, schedules, can_spawn)
                .iter()
                .map(|t| t.name())
                .collect::<Vec<_>>()
        };

        let granted = names(true, true, true);
        assert!(granted.contains(&"reply"), "a returning view holds `reply`");
        for f in ["schedule", "schedules", "unschedule"] {
            assert!(granted.contains(&f), "a scheduling view holds `{f}`");
        }
        for f in ["amnemon", "mnemon", "commit", "verify_commitment"] {
            assert!(granted.contains(&f), "a fueled view holds `{f}`");
        }
        assert!(granted.contains(&"ral"), "the always-tools are present");

        let withheld = names(false, false, false);
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
        for f in ["amnemon", "mnemon", "commit", "verify_commitment"] {
            assert!(
                !withheld.contains(&f),
                "an out-of-fuel view withholds the spawn tool `{f}`"
            );
        }
        assert!(
            withheld.contains(&"ral"),
            "the always-tools survive every axis off"
        );
        assert!(
            withheld.contains(&"agents"),
            "the spawn-management tools stay unconditional even out of fuel"
        );
    }
}
