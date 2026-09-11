//! The tools a request may advertise — `ral`, and `thinking` under
//! `--thinking-tool` — and the spawn plumbing behind `/branch` and the desk's
//! `` agents `start ``.
//!
//! Everything else the model reaches — spawning a sub-agent, messaging one,
//! scheduling a wakeup, replying — is an ordinary ral builtin in
//! `shell_eval/builtins/harness.rs`, answered by [`crate::fleet::desk`].

pub(crate) mod agent;
pub(crate) mod ral;
pub(crate) mod thinking;

pub(crate) use agent::spawn_branch;

use crate::agent::Avatar;
use crate::agent::event::ToolResult;
use crate::bus::Emitter;
use serde_json::Value;

/// One tool: its wire definition, and the dispatch that answers a call to it.
pub(crate) struct Tool {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    run: fn(String, &Value, &mut Avatar, &Emitter) -> ToolResult,
}

impl Tool {
    pub(crate) fn run(
        &self,
        id: String,
        input: &Value,
        session: &mut Avatar,
        emit: &Emitter,
    ) -> ToolResult {
        (self.run)(id, input, session, emit)
    }

    fn wire(&self) -> genai::chat::Tool {
        genai::chat::Tool::new(self.name)
            .with_description(self.description)
            .with_schema((self.schema)())
    }
}

static RAL: Tool = Tool {
    name: ral::NAME,
    description: ral::DESC,
    schema: ral::schema,
    run: ral::dispatch,
};

static THINKING: Tool = Tool {
    name: thinking::NAME,
    description: thinking::DESC,
    schema: thinking::schema,
    run: thinking::dispatch,
};

static OFFER: [&Tool; 2] = [&RAL, &THINKING];

/// What a request advertises and what dispatch recognises are one value, so
/// the two cannot disagree.
#[derive(Clone, Copy, Default)]
pub(crate) struct Toolset(&'static [&'static Tool]);

impl Toolset {
    /// `ral`, and the `thinking` relay when asked for.
    pub(crate) fn offered(thinking: bool) -> Self {
        Self(if thinking { &OFFER } else { &OFFER[..1] })
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn get(self, name: &str) -> Option<&'static Tool> {
        self.0.iter().copied().find(|tool| tool.name == name)
    }

    pub(crate) fn wire(self) -> impl Iterator<Item = genai::chat::Tool> {
        self.0.iter().map(|tool| tool.wire())
    }
}

/// A required string field of the model's input, or the reason it is not one.
pub(crate) fn required_str<'a>(input: &'a Value, field: &str) -> Result<&'a str, String> {
    input
        .as_object()
        .ok_or_else(|| "tool input is not a JSON object".to_string())?
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string field `{field}`"))
}

/// The result a malformed call earns.
pub(crate) fn input_error(reason: &str) -> String {
    format!("tool input error: {reason}\nexpected an object matching the tool's schema")
}
