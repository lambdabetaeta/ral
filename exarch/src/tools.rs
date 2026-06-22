//! Tool registry — one entry per tool the model may call.
//!
//! A [`Tool`] knows how to advertise itself to the provider (name,
//! description, JSON schema) and how to dispatch one parsed JSON input
//! against a live [`Session`], returning a [`SessionToolResult`].  Every tool
//! returns synchronously: a tool that forks a child session (`agent`) launches
//! a detached peer and returns a start receipt, so dispatch never blocks and
//! there is no join phase.
//!
//! Each tool owns its own input parsing and invalid-input UX —
//! [`Tool::dispatch`] is given a raw `serde_json::Value`, and the tool
//! decides how to read it, what label to show on the rail, and how to
//! report a malformed call.  Adding a tool means writing a sibling
//! module under `tools/` and listing it in [`registry`]; nothing in
//! `provider.rs` or `session.rs` needs to know its shape.

use crate::bus::{Emitter, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use crate::session::Session;
use serde_json::Value;
use std::sync::{Arc, OnceLock};

mod agent;
mod fff;
mod ral;
mod reply;
mod schedule;

/// Which tools a session advertises to the provider and may dispatch — the
/// single source of truth for both advertisement (`provider.complete`) and
/// enforcement ([`Session`]'s dispatch path), replacing the old
/// `advertise_root_only` bool and the `is_subagent` dispatch check.
///
/// The two sets gate on mirror-image axes: the root holds the spawn family
/// but not `reply` (it talks to the user across turns and never returns a
/// value); a peer holds `reply` but not the spawn family (so it can return,
/// and the spawn tree stays one level deep).
#[derive(Clone, Copy)]
pub enum ToolSet {
    /// Every registered tool except `reply` — a root.
    All,
    /// Every tool except the spawn family (`agent` / `agents` /
    /// `agent_cancel`) — a peer; it keeps `reply`, its way of returning.
    NoSpawn,
}

impl ToolSet {
    /// Whether `tool` is advertised and permitted under this set.
    pub(crate) fn allows(&self, tool: &dyn Tool) -> bool {
        match self {
            ToolSet::All => !tool.replies(),
            ToolSet::NoSpawn => !tool.spawns(),
        }
    }
}

/// One registered tool.  The registry stores `Box<dyn Tool>` and
/// dispatches by name; [`Session`] holds no tool-specific knowledge.
pub(crate) trait Tool: Send + Sync {
    /// Stable identifier used by the model and the wire schema.
    fn name(&self) -> &'static str;

    /// One-paragraph description handed to the provider.
    fn desc(&self) -> &'static str;

    /// JSON schema for the tool's input object.  Built once and cached
    /// inside the impl; cheap to call.
    fn schema(&self) -> &'static Value;

    /// True for the spawn family (`agent` / `agents` / `agent_cancel`).
    /// These are withheld from a peer's [`ToolSet`] — both unadvertised and
    /// refused — so a sub-agent cannot spawn its own children and the spawn
    /// tree stays one level deep.  Everything else (including `schedule`, so a
    /// peer may wake itself) defaults to `false`.
    fn spawns(&self) -> bool {
        false
    }

    /// True only for `reply` — the mirror of [`Self::spawns`].  It is
    /// withheld from the *root*'s [`ToolSet`] (unadvertised and refused): the
    /// root talks to the user across turns and never returns a value, so
    /// returning-and-terminating is meaningless there.  A peer holds it.
    fn replies(&self) -> bool {
        false
    }

    /// Read `input`, render the rail header, run the call, and return its
    /// result.  A tool that forks a child session (`agent`) launches a
    /// detached worker and returns a start receipt here — it does not block.
    /// Malformed input is reported by the tool itself via [`invalid_input`]
    /// (or its own equivalent), so this method always produces a result.  A
    /// tool reads the live cancellation token from `session` if it needs it.
    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Session,
        provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult;
}

/// All registered tools, in the order they were added.  Built once.
pub(crate) fn registry() -> &'static [Box<dyn Tool>] {
    static R: OnceLock<Vec<Box<dyn Tool>>> = OnceLock::new();
    R.get_or_init(|| {
        vec![
            Box::new(ral::RalTool),
            Box::new(agent::AgentTool),
            Box::new(agent::AgentsTool),
            Box::new(agent::AgentCancelTool),
            Box::new(schedule::ScheduleTool),
            Box::new(schedule::SchedulesTool),
            Box::new(schedule::UnscheduleTool),
            Box::new(fff::FffTool),
            Box::new(reply::ReplyTool),
        ]
    })
}

/// Look up a registered tool by name.
pub(crate) fn find(name: &str) -> Option<&'static dyn Tool> {
    registry()
        .iter()
        .find(|t| t.name() == name)
        .map(|b| b.as_ref())
}

/// Render the rail header and an error block for a malformed tool
/// call, returning the [`SessionToolResult`] the dispatcher commits.
/// `display` is the partial label the rail should show — typically
/// the field that did parse, or `"<invalid input>"` when nothing did.
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
