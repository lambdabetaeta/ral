//! The tools a request may advertise: their wire definitions alone; dispatch
//! is `agent::tools`'.

use serde_json::{Value, json};

/// One tool's wire definition.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
}

impl ToolSpec {
    pub(crate) fn wire(&self) -> genai::chat::Tool {
        genai::chat::Tool::new(self.name)
            .with_description(self.description)
            .with_schema((self.schema)())
    }
}

/// The one tool every agent is offered, by the name its calls are recorded under.
pub const RAL_TOOL: &str = "ral";

pub static RAL: ToolSpec = ToolSpec {
    name: RAL_TOOL,
    description: "Run a ral shell command in the sandboxed working directory.",
    schema: ral_schema,
};

pub static THINKING: ToolSpec = ToolSpec {
    name: "thinking",
    description: "Use this to record your thinking.",
    schema: thinking_schema,
};

static OFFER: [&ToolSpec; 2] = [&RAL, &THINKING];

/// What a request advertises and what dispatch recognises are one value, so
/// the two cannot disagree.  The default is empty: `--chat`, which offers
/// the model no tool at all.
#[derive(Clone, Copy, Default)]
pub struct Toolset(&'static [&'static ToolSpec]);

impl Toolset {
    /// `ral`, and the `thinking` relay when asked for.
    pub fn offered(thinking: bool) -> Self {
        Self(if thinking { &OFFER } else { &OFFER[..1] })
    }

    pub fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(self, name: &str) -> bool {
        self.0.iter().any(|tool| tool.name == name)
    }

    pub(crate) fn wire(self) -> impl Iterator<Item = genai::chat::Tool> {
        self.0.iter().map(|tool| tool.wire())
    }
}

/// Default wall bound on one call.  A default, not a cap: a call may raise it,
/// and nothing clamps the value it names.
pub(crate) const CALL_TIMEOUT_SECS: u64 = 60;

/// Character cap on `description`; oversize input truncates rather than rejects.
pub(crate) const DESCRIPTION_MAX: usize = 60;

fn ral_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cmd": { "type": "string", "description": "The ral source to evaluate." },
            "description": {
                "type": "string",
                "maxLength": DESCRIPTION_MAX,
                "description": "One line (≤60 chars) stating the script's \
        intent: what it is for, not what it types. Present continuous, e.g. \
        \"Counting TODOs across src/.\". Shown on the rail; no newlines. Do not echo the source, or the mechanics of ral.",
            },
            "timeout_secs": {
                "type": "integer",
                "minimum": 1,
                "description": "Wall-clock limit in seconds; default 60. \
        Raise it for a command known to run long, rather than defer-then-wait. \
        Keep defer for work you can overlap.",
            },
        },
        "required": ["cmd", "description"],
    })
}

fn thinking_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "thought": {
                "type": "string",
                "description": "Thinking.",
            },
        },
        "required": ["thought"],
    })
}
