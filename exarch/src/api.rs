//! LLM provider — one struct, three wire formats.
//!
//! `Provider::step` takes either a fresh user turn or a batch of tool
//! results and streams the assistant's reply, branching once on `kind`
//! to pick Anthropic Messages or OpenAI/OpenRouter Chat Completions
//! shapes.  Text deltas are forwarded to the caller's `on_text`
//! callback as they arrive; tool-use blocks and usage are buffered
//! and returned in `StepOut`.

use crate::cancel;
use clap::ValueEnum;
use serde_json::{Value, json};
use std::time::Duration;

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
    /// Tokens written into the prompt cache this turn.
    pub cache_creation: u64,
    /// Tokens read as cache hits this turn.
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
    /// Single-threaded tokio runtime used to drive the cancel-aware HTTP
    /// request; created once and reused across every `step` and
    /// `compact` call.
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

/// Sentinel embedded in the error string when a request was cancelled
/// by Ctrl-C.  Callers use `is_cancelled` to distinguish a cancel from
/// a transport / API error.
pub const CANCEL_MARKER: &str = "[exarch-cancelled]";

pub fn is_cancelled(err: &str) -> bool {
    err.contains(CANCEL_MARKER)
}

impl Provider {
    fn or_anthropic(&self) -> bool {
        self.kind == ProviderKind::Openrouter && self.model.starts_with("anthropic/")
    }

    pub fn new(kind: ProviderKind, key: String, model: String, system: String) -> Self {
        let or_anthropic = kind == ProviderKind::Openrouter && model.starts_with("anthropic/");
        let history = if kind == ProviderKind::Anthropic {
            Vec::new()
        } else {
            vec![system_msg(&system, or_anthropic)]
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio current-thread runtime");
        let client = reqwest::Client::builder()
            .build()
            .expect("build reqwest client");
        Self { kind, key, model, system, history, runtime, client }
    }

    /// Drive one assistant turn.  Text is forwarded to `on_text` as
    /// SSE deltas arrive; the returned `StepOut` carries any tool-use
    /// blocks the model emitted plus the per-turn token usage.
    pub fn step<F: FnMut(&str)>(
        &mut self,
        input: Step,
        on_text: &mut F,
    ) -> Result<StepOut, String> {
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
        let (mut body, headers) = if anthropic {
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
            // For Anthropic models via OpenRouter, stamp the last message with
            // cache_control to anchor a cumulative cache breakpoint each turn.
            let messages_send = if self.or_anthropic() {
                let mut msgs = self.history.clone();
                stamp_last_message_cache(&mut msgs);
                msgs
            } else {
                self.history.clone()
            };
            (
                json!({
                    "model": self.model, "max_tokens": MAX_TOKENS, "messages": messages_send,
                    "tools": [{ "type": "function", "function": {
                        "name": TOOL_NAME, "description": TOOL_DESC, "parameters": schema,
                    }}],
                    "usage": { "include": true },
                    "stream_options": { "include_usage": true },
                }),
                vec![("authorization", format!("Bearer {}", self.key))],
            )
        };
        body["stream"] = json!(true);
        let url = self.kind.info().3;

        let mut tool_calls = Vec::new();
        let done;
        let (input_tok, output_tok, cache_creation, cache_read, dollars);

        if anthropic {
            let (content, stop, usage_raw) = self.runtime.block_on(
                stream_anthropic(&self.client, url, &headers, body, on_text),
            )?;
            let g = |k: &str| usage_raw.get(k).and_then(|n| n.as_u64()).unwrap_or(0);
            for block in content.as_array().into_iter().flatten() {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let id = block.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                    let input = block.get("input");
                    let cmd = input.and_then(|i| i.get("cmd"))
                        .and_then(|s| s.as_str()).unwrap_or("").to_string();
                    let audit = input.and_then(|i| i.get("audit"))
                        .and_then(|b| b.as_bool()).unwrap_or(false);
                    tool_calls.push(ToolCall { id, cmd, audit });
                }
            }
            self.history.push(json!({ "role": "assistant", "content": content }));
            done = stop != "tool_use";
            (input_tok, output_tok) = (g("input_tokens"), g("output_tokens"));
            (cache_creation, cache_read) = (
                g("cache_creation_input_tokens"),
                g("cache_read_input_tokens"),
            );
            dollars = dollars_for(&self.model, input_tok, output_tok, cache_creation, cache_read);
        } else {
            let (msg, finish, usage_raw) = self.runtime.block_on(
                stream_openai(&self.client, url, &headers, body, on_text),
            )?;
            let g = |k: &str| usage_raw.get(k).and_then(|n| n.as_u64()).unwrap_or(0);
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
            (input_tok, output_tok) = (g("prompt_tokens"), g("completion_tokens"));
            // Both OpenAI and OpenRouter report cache hits in prompt_tokens_details.
            // OpenRouter also reports cache_write_tokens for Anthropic models; OpenAI
            // caching is fully automatic so only cached_tokens appears there.
            let det = usage_raw.get("prompt_tokens_details");
            let gd = |k: &str| det.and_then(|d| d.get(k)).and_then(|n| n.as_u64()).unwrap_or(0);
            (cache_creation, cache_read) = (gd("cache_write_tokens"), gd("cached_tokens"));
            dollars = usage_raw.get("cost").and_then(|n| n.as_f64())
                .unwrap_or_else(|| dollars_for(&self.model, input_tok, output_tok, 0, 0));
        }
        Ok(StepOut {
            tool_calls, done,
            usage: Usage { input: input_tok, output: output_tok, cache_creation, cache_read, dollars },
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Reset conversation history and cost counters; the system prompt
    /// and provider configuration are preserved.
    pub fn clear_history(&mut self) {
        self.history.clear();
        if self.kind != ProviderKind::Anthropic {
            self.history.push(system_msg(&self.system, self.or_anthropic()));
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
        let resp = self.runtime.block_on(post_buffered(&self.client, self.kind.info().3, &headers, body))?;
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
            self.history.push(system_msg(&self.system, self.or_anthropic()));
            self.history.push(json!({ "role": "user", "content": body_msg }));
        }
        Ok((inp, out, dollars))
    }
}

/// POST and read the full body as JSON.  Used by `compact`, which has
/// no callback to deliver and no need for the streaming machinery.
async fn post_buffered(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
    body: Value,
) -> Result<Value, String> {
    let resp = post_open(client, url, headers, body).await?;
    let read = resp.bytes();
    tokio::pin!(read);
    let bytes = tokio::select! {
        biased;
        _ = wait_for_cancel() => return Err(format!("{CANCEL_MARKER} cancelled mid-response")),
        r = &mut read => r.map_err(|e| e.to_string())?,
    };
    serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())
}

/// Send the request, race against cancel until the response head is
/// available, and surface non-2xx as a formatted error.  Common entry
/// for both the streaming and buffered paths.
async fn post_open(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
    body: Value,
) -> Result<reqwest::Response, String> {
    let mut req = client.post(url).header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let send = req.json(&body).send();
    tokio::pin!(send);
    let resp = tokio::select! {
        biased;
        _ = wait_for_cancel() => return Err(format!("{CANCEL_MARKER} cancelled before response")),
        r = &mut send => r.map_err(|e| e.to_string())?,
    };
    let status = resp.status();
    if !status.is_success() {
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        return Err(format!("api {}: {}", status.as_u16(), String::from_utf8_lossy(&bytes)));
    }
    Ok(resp)
}

/// Stream an Anthropic Messages response.  Forwards text deltas to
/// `on_text` and assembles the final `content` array (text and
/// tool_use blocks in the order the model emitted them) so the caller
/// can fold it into history exactly as the buffered path used to.
async fn stream_anthropic<F: FnMut(&str)>(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
    body: Value,
    on_text: &mut F,
) -> Result<(Value, String, Value), String> {
    let mut resp = post_open(client, url, headers, body).await?;

    let mut blocks: Vec<Option<Block>> = Vec::new();
    let mut stop = String::new();
    let mut usage = json!({});
    let mut sse = Sse::new();

    loop {
        let chunk = tokio::select! {
            biased;
            _ = wait_for_cancel() => return Err(format!("{CANCEL_MARKER} cancelled mid-stream")),
            c = resp.chunk() => c.map_err(|e| e.to_string())?,
        };
        let Some(bytes) = chunk else { break };
        sse.feed(&bytes, |event, data| {
            let v: Value = serde_json::from_str(data)
                .map_err(|e| format!("sse json: {e}"))?;
            match event {
                Some("content_block_start") => {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    let block = &v["content_block"];
                    grow(&mut blocks, idx + 1);
                    blocks[idx] = match block["type"].as_str().unwrap_or("") {
                        "text" => Some(Block::Text(String::new())),
                        "tool_use" => Some(Block::Tool {
                            id: block["id"].as_str().unwrap_or("").into(),
                            name: block["name"].as_str().unwrap_or("").into(),
                            args: String::new(),
                        }),
                        _ => None,
                    };
                }
                Some("content_block_delta") => {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    let d = &v["delta"];
                    match (d["type"].as_str().unwrap_or(""), blocks.get_mut(idx).and_then(Option::as_mut)) {
                        ("text_delta", Some(Block::Text(buf))) => {
                            if let Some(t) = d["text"].as_str() {
                                buf.push_str(t);
                                on_text(t);
                            }
                        }
                        ("input_json_delta", Some(Block::Tool { args, .. })) => {
                            if let Some(p) = d["partial_json"].as_str() {
                                args.push_str(p);
                            }
                        }
                        _ => {}
                    }
                }
                Some("message_start") => {
                    if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                        merge_into(&mut usage, u);
                    }
                }
                Some("message_delta") => {
                    if let Some(s) = v["delta"]["stop_reason"].as_str() {
                        stop = s.to_string();
                    }
                    if let Some(u) = v.get("usage") {
                        merge_into(&mut usage, u);
                    }
                }
                _ => {}
            }
            Ok(())
        })?;
    }

    let content: Vec<Value> = blocks.into_iter().filter_map(|b| match b? {
        Block::Text(text) => Some(json!({ "type": "text", "text": text })),
        Block::Tool { id, name, args } => {
            let input = serde_json::from_str::<Value>(&args).unwrap_or(json!({}));
            Some(json!({ "type": "tool_use", "id": id, "name": name, "input": input }))
        }
    }).collect();
    Ok((Value::Array(content), stop, usage))
}

/// Stream an OpenAI/OpenRouter Chat Completions response.  Same idea
/// as `stream_anthropic` but on the chat-completions wire shape: text
/// arrives in `choices[0].delta.content`, tool calls accumulate in
/// `choices[0].delta.tool_calls`, and the final usage block is opted
/// into via `stream_options.include_usage`.
async fn stream_openai<F: FnMut(&str)>(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
    body: Value,
    on_text: &mut F,
) -> Result<(Value, String, Value), String> {
    let mut resp = post_open(client, url, headers, body).await?;

    let mut content = String::new();
    let mut tools: Vec<Option<ToolAcc>> = Vec::new();
    let mut finish = String::new();
    let mut usage = json!({});
    let mut sse = Sse::new();

    loop {
        let chunk = tokio::select! {
            biased;
            _ = wait_for_cancel() => return Err(format!("{CANCEL_MARKER} cancelled mid-stream")),
            c = resp.chunk() => c.map_err(|e| e.to_string())?,
        };
        let Some(bytes) = chunk else { break };
        sse.feed(&bytes, |_event, data| {
            if data == "[DONE]" { return Ok(()); }
            let v: Value = serde_json::from_str(data)
                .map_err(|e| format!("sse json: {e}"))?;
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                usage = u.clone();
            }
            let Some(choice) = v["choices"].as_array().and_then(|a| a.first()) else {
                return Ok(());
            };
            if let Some(fr) = choice["finish_reason"].as_str() {
                finish = fr.to_string();
            }
            let delta = &choice["delta"];
            if let Some(t) = delta["content"].as_str() {
                content.push_str(t);
                on_text(t);
            }
            if let Some(tcs) = delta["tool_calls"].as_array() {
                for tc in tcs {
                    let i = tc["index"].as_u64().unwrap_or(0) as usize;
                    grow(&mut tools, i + 1);
                    let slot = tools[i].get_or_insert_with(ToolAcc::default);
                    if let Some(id) = tc["id"].as_str() {
                        if slot.id.is_empty() { slot.id = id.into(); }
                    }
                    let f = &tc["function"];
                    if let Some(name) = f["name"].as_str() {
                        if slot.name.is_empty() { slot.name = name.into(); }
                    }
                    if let Some(args) = f["arguments"].as_str() {
                        slot.args.push_str(args);
                    }
                }
            }
            Ok(())
        })?;
    }

    let mut msg = json!({ "role": "assistant" });
    msg["content"] = if content.is_empty() { Value::Null } else { Value::String(content) };
    let calls: Vec<Value> = tools.into_iter().flatten().map(|t| json!({
        "id": t.id, "type": "function",
        "function": { "name": t.name, "arguments": t.args },
    })).collect();
    if !calls.is_empty() {
        msg["tool_calls"] = Value::Array(calls);
    }
    Ok((msg, finish, usage))
}

/// One assembled content block in an Anthropic streaming response.
enum Block {
    Text(String),
    Tool { id: String, name: String, args: String },
}

#[derive(Default)]
struct ToolAcc { id: String, name: String, args: String }

/// Grow `v` with `Default`s until it has at least `len` entries.
fn grow<T: Default>(v: &mut Vec<T>, len: usize) {
    while v.len() < len {
        v.push(T::default());
    }
}

/// Shallow object merge: copy every key in `src` into `into`.  Used to
/// fold the input-side and output-side usage halves Anthropic ships in
/// `message_start` and `message_delta` into one object.
fn merge_into(into: &mut Value, src: &Value) {
    let Some(src) = src.as_object() else { return };
    if !into.is_object() {
        *into = json!({});
    }
    let dst = into.as_object_mut().expect("just promoted to object");
    for (k, v) in src {
        dst.insert(k.clone(), v.clone());
    }
}

/// Resolves once the cancel flag is observed.  Polls on a 50ms cadence
/// — the interval is the upper bound on Ctrl-C latency for an in-flight
/// HTTP call, traded against the cost of waking the runtime.
async fn wait_for_cancel() {
    while !cancel::is_set() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Build a Chat Completions system message.  When `cached`, the content is
/// wrapped in a block with `cache_control` so Anthropic caches the static
/// system prefix on the first turn and serves it from cache thereafter.
fn system_msg(text: &str, cached: bool) -> Value {
    if cached {
        json!({ "role": "system", "content": [{ "type": "text", "text": text, "cache_control": { "type": "ephemeral" } }] })
    } else {
        json!({ "role": "system", "content": text })
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

/// Server-Sent Events line buffer.  Feed it bytes; emit one event per
/// blank-line-terminated block of `field: value` lines.  Comments
/// (`:…`) and unknown fields are ignored; multiple `data:` lines per
/// event join with `\n` per the SSE spec.
struct Sse {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl Sse {
    fn new() -> Self {
        Self { buf: Vec::new(), event: None, data: Vec::new() }
    }

    fn feed<F>(&mut self, bytes: &[u8], mut emit: F) -> Result<(), String>
    where
        F: FnMut(Option<&str>, &str) -> Result<(), String>,
    {
        self.buf.extend_from_slice(bytes);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = std::str::from_utf8(&line).unwrap_or("")
                .trim_end_matches('\n')
                .trim_end_matches('\r');
            if line.is_empty() {
                if !self.data.is_empty() {
                    let data = self.data.join("\n");
                    let event = self.event.take();
                    emit(event.as_deref(), &data)?;
                    self.data.clear();
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                self.data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            } else if let Some(rest) = line.strip_prefix("event:") {
                self.event = Some(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
        }
        Ok(())
    }
}

/// Per-million-token prices for known models.  OpenRouter populates a
/// dollar `cost` field directly; for direct API users we look it up.
///
/// Anthropic bills cache writes at 1.25× and cache reads at 0.1× the
/// base input rate.
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

