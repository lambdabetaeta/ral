//! Tool registry — one entry per tool the model may call.
//!
//! A [`Tool`] knows how to advertise itself to the provider (name,
//! description, JSON schema) and how to dispatch one parsed JSON
//! input against a live [`Session`].  Synchronous tools return
//! [`Staged::Done`]; tools that fork a child session return
//! [`Staged::Spawned`] and are joined by the parent's dispatch loop.
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
use crate::session::{Session, Staged};
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use std::thread;

mod agent;
mod fff;
mod ral;

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

    /// True for tools only the root session may call.  Root-only tools
    /// are not advertised to forked sub-agents and are refused if one
    /// attempts the call.  `agent` is root-only: a sub-agent that could
    /// spawn its own children turns a one-line task into a recursion of
    /// duplicated sessions, so the spawn tree stays one level deep.
    fn root_only(&self) -> bool {
        false
    }

    /// Read `input`, render the rail header, and run the call.
    /// Synchronous tools return [`Staged::Done`] inline; concurrent
    /// tools spawn on `scope` and return [`Staged::Spawned`].
    /// Malformed input is reported by the tool itself via
    /// [`invalid_input`] (or its own equivalent), so this method
    /// always succeeds in producing a [`Staged`].
    ///
    /// `token` is the root turn's cancellation token
    /// ([`crate::cancel::Token`]); a tool that forks a child session
    /// (`agent`) hands the child a clone so an Esc cancels the whole tree.
    #[allow(clippy::too_many_arguments)] // each is distinct dispatch context, not a bundle
    fn dispatch<'scope, 'env: 'scope>(
        &self,
        id: String,
        input: Value,
        session: &mut Session,
        provider: &'env Arc<Provider>,
        token: &'env crate::cancel::Token,
        emit: &Emitter,
        scope: &'scope thread::Scope<'scope, 'env>,
    ) -> Staged<'scope>;
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
            Box::new(fff::FffTool),
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
/// call, returning the [`Staged`] that satisfies the dispatcher.
/// `display` is the partial label the rail should show — typically
/// the field that did parse, or `"<invalid input>"` when nothing did.
pub(super) fn invalid_input<'scope>(
    id: String,
    tool: &'static str,
    display: &str,
    reason: &str,
    emit: &Emitter,
) -> Staged<'scope> {
    emit.emit(Kind::ToolCall {
        tool,
        cmd: display.to_string(),
        summary: None,
    });
    let msg = format!("tool input error: {reason}\nexpected an object matching the tool's schema");
    emit.emit(Kind::ToolResult(msg.clone()));
    Staged::Done(SessionToolResult { id, content: msg })
}
