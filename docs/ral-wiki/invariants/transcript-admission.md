# The transcript admits only sendable messages

Every message committed to a session's [[map/exarch/frontend|event log]]
serialises to a request **every supported provider accepts.** The event log is
the single source of truth for the next request; once a malformed or empty
message is committed, it is re-serialised on every subsequent round-trip, so a
single bad commit can wedge the whole run on a strict backend (Anthropic 400s,
the run dies — misclassified as fatal). The protocol state machine
([[map/exarch/frontend|`event.rs`]]) enforces *role alternation*; this invariant
enforces *per-message admissibility* at the one place messages enter the log:
the `deliberate` commit boundary in [[map/exarch/agent|agent]].

Three commit-time obligations, all in `Agent::deliberate` (the deep-review X-tags):

- **Tool-call arguments are objects (X2).** A `ContentPart::ToolCall` whose
  `fn_arguments` is not a JSON object is repaired to `{}` by `admit_assistant`
  before `append_assistant`. genai's Anthropic adapter repairs only a `null`
  argument, so a bare string / number / array re-serialises verbatim and strict
  backends reject every later request carrying it.
- **No empty assistant message (X7).** An assistant turn with no substantive
  part — no tool call, no non-empty text, no binary — would serialise as
  `"content": []` (or an empty text block) and is rejected once a later message
  (the empty-turn nudge's user prompt) makes it non-final. `admit_assistant`
  substitutes a short stub; the deliberation still surfaces as `Empty` so the
  nudge recovers it.
- **A `MaxTokens` turn with captured tool calls dispatches, not strands (X6).**
  Returning `Truncated` there leaves the assistant message carrying `tool_ids`
  and the session in `AwaitingToolResults`, where the nudge's next `append_user`
  fails "tool results pending". `deliberate` instead dispatches the captured
  calls and continues the loop; only a `MaxTokens` turn with *no* tool call
  raises `Truncated` to nudge.

Two adjacent obligations keep the invariant whole:

- **Auto-compaction runs where it can (X1).** Compaction needs `ReadyForUser`
  (`can_compact`), which holds at the top of `deliberate`, never mid-loop in
  `AwaitingAssistantAfterToolResults` — the prior placement was dead code.
- **A JSON-body 4xx is classified, not lost (X3).** `parse_4xx_status` matches
  both the `status: <code>` token and a JSON `"code": <code>` body, so an
  OpenRouter-shaped `{"code":400,…}` becomes a structured `Api` error rather
  than an opaque `Other`.

The hard rule: a new shape of committed message must pass through the same
admission step — never append an assistant message (or a tool result) the
serialiser would render in a form a supported provider rejects. This is the
exarch analogue of the wire-hop discipline
([[map/core/transport|exhaustive maps]]): the seam admits only validated,
complete values.

See also [[invariants/turn-ends-ready|exchange-ends-ready]] (the role-alternation
half), [[map/exarch/agent|agent]] (`deliberate`, `admit_assistant`, `compact`),
[[map/exarch/provider|provider]] (`from_genai`, `parse_4xx_status`).
