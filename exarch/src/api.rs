//! LLM provider — one struct, three wire formats.
//!
//! `Provider::step` takes either a fresh user turn or a batch of tool
//! results and returns the assistant's text plus any tool calls it
//! requests, branching once on `kind` to choose Anthropic Messages
//! or OpenAI/OpenRouter Chat Completions wire shapes.

use clap::ValueEnum;
use serde_json::{Value, json};

const MAX_TOKENS: u32 = 4096;
const TOOL_NAME: &str = "shell";
const TOOL_DESC: &str = "Run a ral shell command in the sandboxed working directory.";

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq)]
pub enum ProviderKind {
    Anthropic,
    Openai,
    Openrouter,
}

impl ProviderKind {
    /// `(label, default_model, key_env, url)` for this provider.
    pub fn info(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::Anthropic  => ("anthropic",  "claude-opus-4-7",          "ANTHROPIC_API_KEY",  "https://api.anthropic.com/v1/messages"),
            Self::Openai     => ("openai",     "gpt-5.5",                  "OPENAI_API_KEY",     "https://api.openai.com/v1/chat/completions"),
            Self::Openrouter => ("openrouter", "anthropic/claude-opus-4.7","OPENROUTER_API_KEY", "https://openrouter.ai/api/v1/chat/completions"),
        }
    }
}

pub enum Step {
    User(String),
    ToolResults(Vec<(String, String)>),
}

/// One tool invocation extracted from the model's reply.  `audit`
/// asks the exarch to wrap evaluation in ral's audit scope and return
/// the per-command exec tree as JSON in the tool result.
pub struct ToolCall {
    pub id: String,
    pub cmd: String,
    pub audit: bool,
}

#[derive(Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    /// Tokens written into the prompt cache this turn (Anthropic only).
    pub cache_creation: u64,
    /// Tokens read as cache hits this turn (Anthropic only).
    pub cache_read: u64,
    pub dollars: f64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_creation += rhs.cache_creation;
        self.cache_read += rhs.cache_read;
        self.dollars += rhs.dollars;
    }
}

pub struct StepOut {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub done: bool,
    pub usage: Usage,
}

pub struct Provider {
    kind: ProviderKind,
    key: String,
    model: String,
    system: String,
    history: Vec<Value>,
}

impl Provider {
    pub fn new(kind: ProviderKind, key: String, model: String, system: String) -> Self {
        let history = if kind == ProviderKind::Anthropic {
            Vec::new()
        } else {
            vec![json!({ "role": "system", "content": system.clone() })]
        };
        Self { kind, key, model, system, history }
    }

    pub fn step(&mut self, input: Step) -> Result<StepOut, String> {
        let schema = json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "The ral source to evaluate." },
                "audit": {
                    "type": "boolean",
                    "description": "If true, wrap evaluation in ral's audit scope and \
return the captured execution tree as JSON.  Use for diagnosing denials, \
unexpected argv, or pipeline behaviour.  Skip for routine commands.",
                },
            },
            "required": ["cmd"],
        });
        let anthropic = self.kind == ProviderKind::Anthropic;
        let (body, headers) = if anthropic {
            self.history.push(match input {
                Step::User(s) => json!({ "role": "user", "content": s }),
                Step::ToolResults(rs) => json!({
                    "role": "user",
                    "content": rs.into_iter().map(|(id, out)| json!({
                        "type": "tool_result", "tool_use_id": id, "content": out,
                    })).collect::<Vec<_>>(),
                }),
            });
            // Stamp cache_control on the system block, the (single) tool
            // definition, and the last message before sending.  The first
            // two cache the static prefix; the third refreshes a moving
            // cumulative breakpoint each turn.  Anthropic accepts up to
            // four breakpoints; we use three.
            let mut messages_send = self.history.clone();
            stamp_last_message_cache(&mut messages_send);
            (
                json!({
                    "model": self.model,
                    "max_tokens": MAX_TOKENS,
                    "system": [{
                        "type": "text", "text": self.system,
                        "cache_control": { "type": "ephemeral" },
                    }],
                    "tools": [{
                        "name": TOOL_NAME, "description": TOOL_DESC, "input_schema": schema,
                        "cache_control": { "type": "ephemeral" },
                    }],
                    "messages": messages_send,
                }),
                vec![("x-api-key", self.key.clone()), ("anthropic-version", "2023-06-01".into())],
            )
        } else {
            match input {
                Step::User(s) => self.history.push(json!({ "role": "user", "content": s })),
                Step::ToolResults(rs) => for (id, out) in rs {
                    self.history.push(json!({ "role": "tool", "tool_call_id": id, "content": out }));
                },
            }
            (
                json!({
                    "model": self.model, "max_tokens": MAX_TOKENS, "messages": self.history,
                    "tools": [{ "type": "function", "function": {
                        "name": TOOL_NAME, "description": TOOL_DESC, "parameters": schema,
                    }}],
                    "usage": { "include": true },
                }),
                vec![("authorization", format!("Bearer {}", self.key))],
            )
        };

        let resp = http_post(self.kind.info().3, &headers, body)?;
        let u = resp.get("usage");
        let g = |k: &str| u.and_then(|u| u.get(k)).and_then(|n| n.as_u64()).unwrap_or(0);

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let done;
        let (input, output, cache_creation, cache_read, dollars);

        if anthropic {
            let content = resp.get("content").cloned().unwrap_or(json!([]));
            let stop = resp.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("");
            for block in content.as_array().into_iter().flatten() {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() { text.push('\n'); }
                        text.push_str(t);
                    },
                    Some("tool_use") => {
                        let id = block.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                        let input = block.get("input");
                        let cmd = input.and_then(|i| i.get("cmd"))
                            .and_then(|s| s.as_str()).unwrap_or("").to_string();
                        let audit = input.and_then(|i| i.get("audit"))
                            .and_then(|b| b.as_bool()).unwrap_or(false);
                        tool_calls.push(ToolCall { id, cmd, audit });
                    }
                    _ => {}
                }
            }
            self.history.push(json!({ "role": "assistant", "content": content }));
            done = stop != "tool_use";
            (input, output) = (g("input_tokens"), g("output_tokens"));
            (cache_creation, cache_read) = (
                g("cache_creation_input_tokens"),
                g("cache_read_input_tokens"),
            );
            dollars = dollars_for(&self.model, input, output, cache_creation, cache_read);
        } else {
            let choice = resp.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first())
                .ok_or_else(|| format!("no choices in response: {resp}"))?;
            let msg = choice.get("message").cloned().unwrap_or(json!({}));
            let finish = choice.get("finish_reason").and_then(|s| s.as_str()).unwrap_or("");
            text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
            if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let id = tc.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                    let args = tc.get("function").and_then(|f| f.get("arguments"))
                        .and_then(|s| s.as_str()).unwrap_or("{}");
                    let parsed = serde_json::from_str::<Value>(args).ok();
                    let cmd = parsed.as_ref()
                        .and_then(|v| v.get("cmd").and_then(|s| s.as_str()).map(String::from))
                        .unwrap_or_default();
                    let audit = parsed.as_ref()
                        .and_then(|v| v.get("audit").and_then(|b| b.as_bool()))
                        .unwrap_or(false);
                    tool_calls.push(ToolCall { id, cmd, audit });
                }
            }
            self.history.push(msg);
            done = finish != "tool_calls" && tool_calls.is_empty();
            (input, output) = (g("prompt_tokens"), g("completion_tokens"));
            (cache_creation, cache_read) = (0, 0);
            dollars = u.and_then(|u| u.get("cost")).and_then(|n| n.as_f64())
                .unwrap_or_else(|| dollars_for(&self.model, input, output, 0, 0));
        }
        Ok(StepOut { text, tool_calls, done, usage: Usage { input, output, cache_creation, cache_read, dollars } })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Reset conversation history and cost counters; the system prompt
    /// and provider configuration are preserved.
    pub fn clear_history(&mut self) {
        self.history.clear();
        if self.kind != ProviderKind::Anthropic {
            self.history.push(json!({ "role": "system", "content": self.system.clone() }));
        }
    }

    /// Remove the last history entry if it is an assistant message whose
    /// content contains `tool_use` blocks with no following `tool_result`.
    ///
    /// Call this when a turn loop is aborted mid-tool-call (e.g. max turns
    /// reached).  Without it, the orphaned `tool_use` blocks cause the next
    /// `step` to fail with an Anthropic 400.
    pub fn trim_last_if_tool_use(&mut self) {
        if let Some(last) = self.history.last() {
            let is_assistant = last.get("role").and_then(|r| r.as_str()) == Some("assistant");
            let has_tool_use = last.get("content")
                .and_then(|c| c.as_array())
                .is_some_and(|blocks| blocks.iter().any(|b| {
                    b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                }));
            if is_assistant && has_tool_use {
                self.history.pop();
            }
        }
    }

    /// Total bytes of message JSON in the conversation history — used
    /// as a rough proxy for token count when deciding whether to
    /// compact (≈ 1 token per 3-4 bytes of mixed text/code).
    pub fn history_bytes(&self) -> usize {
        self.history
            .iter()
            .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
            .sum()
    }

    /// Run a one-shot non-tool summary call against the current history,
    /// then replace the history with a single user message carrying the
    /// summary so subsequent turns pay for a much shorter prefix.
    pub fn compact(&mut self) -> Result<(u64, u64, f64), String> {
        let prompt = "Summarise the conversation so far in one paragraph: \
the user's task, what has been tried, what worked, what state the \
shell is in (cwd, env, defined names), and any open subtasks.  \
Return only the summary, no preamble.";
        let mut messages = self.history.clone();
        messages.push(json!({ "role": "user", "content": prompt }));

        let anthropic = self.kind == ProviderKind::Anthropic;
        let body = if anthropic {
            json!({ "model": self.model, "max_tokens": 1024, "system": self.system, "messages": messages })
        } else {
            json!({ "model": self.model, "max_tokens": 1024, "messages": messages })
        };
        let headers = if anthropic {
            vec![("x-api-key", self.key.clone()), ("anthropic-version", "2023-06-01".into())]
        } else {
            vec![("authorization", format!("Bearer {}", self.key))]
        };
        let resp = http_post(self.kind.info().3, &headers, body)?;
        let summary = if anthropic {
            resp.get("content").and_then(|c| c.as_array())
                .and_then(|a| a.iter().find_map(|b| b.get("text").and_then(|t| t.as_str())))
                .unwrap_or("").to_string()
        } else {
            resp.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first())
                .and_then(|c| c.get("message")).and_then(|m| m.get("content"))
                .and_then(|c| c.as_str()).unwrap_or("").to_string()
        };
        let u = resp.get("usage");
        let g = |k: &str| u.and_then(|u| u.get(k)).and_then(|n| n.as_u64()).unwrap_or(0);
        let (inp, out) = if anthropic {
            (g("input_tokens"), g("output_tokens"))
        } else {
            (g("prompt_tokens"), g("completion_tokens"))
        };
        let dollars = u.and_then(|u| u.get("cost")).and_then(|n| n.as_f64())
            .unwrap_or_else(|| dollars_for(&self.model, inp, out, 0, 0));

        self.history.clear();
        let body_msg = format!("Summary of prior work in this session:\n\n{summary}");
        if anthropic {
            self.history.push(json!({ "role": "user", "content": body_msg }));
        } else {
            self.history.push(json!({ "role": "system", "content": self.system.clone() }));
            self.history.push(json!({ "role": "user", "content": body_msg }));
        }
        Ok((inp, out, dollars))
    }
}

fn http_post(url: &str, headers: &[(&str, String)], body: Value) -> Result<Value, String> {
    let mut req = ureq::post(url).set("content-type", "application/json");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_json(body) {
        Ok(r) => r.into_json::<Value>().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(code, r)) => Err(format!("api {code}: {}", r.into_string().unwrap_or_default())),
        Err(e) => Err(e.to_string()),
    }
}

/// In-place: decorate the last block of the last message with
/// `cache_control: ephemeral` so Anthropic caches the cumulative
/// prefix.  Idempotent on already-block content; converts a string
/// content into block form first.
fn stamp_last_message_cache(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else { return };
    let Some(content) = last.get_mut("content") else { return };
    if let Some(s) = content.as_str().map(String::from) {
        *content = json!([{
            "type": "text", "text": s,
            "cache_control": { "type": "ephemeral" },
        }]);
        return;
    }
    if let Some(arr) = content.as_array_mut()
        && let Some(last_block) = arr.last_mut()
        && let Some(obj) = last_block.as_object_mut()
    {
        obj.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    }
}


/// Per-million-token prices for known models.  OpenRouter populates a
/// dollar `cost` field directly; for direct API users we look it up.
///
/// Anthropic bills cache writes at 1.25× and cache reads at 0.1× the
/// base input rate; `cache_creation` and `cache_read` are zero for
/// OpenAI/OpenRouter paths.
fn dollars_for(model: &str, input: u64, output: u64, cache_creation: u64, cache_read: u64) -> f64 {
    let (pi, po) = match model {
        "claude-opus-4-7"   => (15.0, 75.0),
        "claude-sonnet-4-6" => (3.0, 15.0),
        "claude-haiku-4-5"  => (1.0, 5.0),
        "gpt-5.5"           => (1.25, 10.0),
        "gpt-5.5-pro"       => (15.0, 60.0),
        _ => return 0.0,
    };
    (input as f64 * pi
        + cache_creation as f64 * pi * 1.25
        + cache_read as f64 * pi * 0.1
        + output as f64 * po)
        / 1_000_000.0
}
