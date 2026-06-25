//! Tool registry — one entry per tool the model may call.
//!
//! A [`Tool`] knows how to advertise itself to the provider (name,
//! description, JSON schema) and how to dispatch one parsed JSON input
//! against a live [`Agent`], returning a [`SessionToolResult`].  Every tool
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

use crate::agent::Agent;
use crate::bus::{Emitter, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use serde_json::Value;
use std::sync::{Arc, OnceLock};

mod agent;
mod fff;
mod ral;
mod reply;
mod schedule;

/// Which tools an agent advertises to the provider and may dispatch — the
/// single source of truth for both advertisement (`provider.complete`) and
/// enforcement ([`Agent`]'s dispatch path).
///
/// One axis decides membership: whether the agent **returns** a value
/// (`reply`).  Spawning is now universal — every agent may spawn, so the tree
/// is unbounded in depth ([[decisions/260624_uniform-agent-nodes]], superseding
/// the depth-1 `spawns()` axis) — leaving exactly two sets:
///
/// - the **conversing** set (the interactive trunk): spawns, but withholds
///   `reply`, because it converses with the user across turns and never hands
///   a value back;
/// - the **returning** set (everyone else — a headless trunk and every
///   sub-agent at any depth): spawns and holds `reply`, its way of returning.
#[derive(Clone, Copy)]
pub struct ToolSet {
    returns: bool,
    schedules: bool,
}

impl ToolSet {
    /// The conversing set (the interactive trunk): withholds `reply`.
    /// `schedules` carries the session's self-wakeup grant (`--allow-schedule`).
    pub(crate) fn conversing(schedules: bool) -> Self {
        Self {
            returns: false,
            schedules,
        }
    }

    /// The returning set (a headless trunk and every sub-agent): holds `reply`.
    /// `schedules` carries the self-wakeup grant, inherited from the parent on
    /// a fork.
    pub(crate) fn returning(schedules: bool) -> Self {
        Self {
            returns: true,
            schedules,
        }
    }

    /// Whether this set grants self-scheduling — read by a fork to inherit the
    /// parent's wakeup authority.
    pub(crate) fn grants_schedule(&self) -> bool {
        self.schedules
    }

    /// Whether `tool` is advertised and permitted under this set.  Two
    /// orthogonal gates: a replier needs the *returns* axis; the self-wakeup
    /// family needs the *schedules* axis (the `--allow-schedule` grant).  Every
    /// other tool — the spawn family included — is universally allowed.
    pub(crate) fn allows(&self, tool: &dyn Tool) -> bool {
        (!tool.replies() || self.returns) && (!tool.schedules() || self.schedules)
    }
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

    /// True only for `reply` — the *returns* axis of the [`ToolSet`].  It is
    /// held by every returning agent (a sub-agent at any depth, and a headless
    /// trunk) and withheld only from the interactive trunk (unadvertised and
    /// refused), which talks to the user across turns and never returns a
    /// value, so returning-and-terminating is meaningless there.
    fn replies(&self) -> bool {
        false
    }

    /// True for the self-wakeup family (`schedule`, `schedules`, `unschedule`)
    /// — the *schedules* axis of the [`ToolSet`].  Advertised and dispatched
    /// only when the session holds the `--allow-schedule` grant; otherwise the
    /// whole family is unadvertised (and refused on the defensive path).
    fn schedules(&self) -> bool {
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
        session: &mut Agent,
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
