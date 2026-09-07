# The transcript admits only sendable messages

Every message committed to a session's
[[internals/session-record|record-backed model projection]] serialises to a
request **every supported provider accepts.** That projection is the single
source of truth for the next request; once a malformed or empty message is
committed, it is re-serialised on every subsequent round-trip, so a
single bad commit can wedge the whole run on a strict backend (Anthropic 400s,
the run dies — misclassified as fatal). The protocol state machine
([[map/exarch/agent|`agent/event.rs`]]) enforces *sequencing* — above all that a
`tool_use` block is answered; this invariant enforces *per-message
admissibility* at the one place messages enter the log: the `deliberate` commit
boundary in [[map/exarch/agent|agent]].

Sequencing is **not** strict user/assistant alternation, and never was.
Consecutive same-role messages are routine: a session that has evicted sends
the head marker — the harness's user-voice index of what left — as message 0,
immediately before the oldest surviving user turn's prompt, an
inherited import can end on a user message with the child's launch prompt
behind it, and an abandoned exchange's note is a user message before the prompt
that replaced it ([[invariants/turn-ends-ready|exchange-ends-ready]]). genai's
Anthropic adapter pushes one JSON object per `ChatMessage` and merges nothing,
so those reach the provider as written. What the wire actually constrains is
the tool-call *pairing* — a structural relation between content blocks — which
is why `AwaitingToolResults` and `validate_result_ids` exist, and why a closed
exchange that never settles is dropped rather than sent: it would carry a
dangling `tool_use`.

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

Three adjacent obligations keep the invariant whole:

- **Eviction runs where it can (X1).** It needs only `Memo::can_evict`
  (`is_ready` → `admits_new_turn`) — false solely at `AwaitingToolResults`,
  strictly weaker than `ReadyForUser`: an edit may land at any rest but a
  batch in flight, because outstanding tool calls name the assistant frame
  their results answer and nothing may come between. This is why the harness
  weighs the context at **every turn boundary**, not at `deliberate` entry
  ([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]). What keeps
  a cut off the work in hand is not this predicate but `plan_eviction`'s own
  shape — it draws its candidates from every resident turn *but the last* —
  and `validate_edit`, which refuses that turn by name.
- **`Inherited` stands only at a fork's opening.** `Admission` refuses
  `Protocol::Inherited` anywhere but the first protocol record of a child log:
  at `ReadyForUser`, with the turn table still empty, and never twice. The
  record is
  sequencing-neutral — it commits no message and advances no state — so the
  rule cannot be read off a `State` alone; it is admission's own, and it is
  what makes "a log has at most one ancestry, fixed at the fork that opened it"
  a property of the file rather than a habit of the writer
  ([[decisions/260906_context-rollover|context-rollover]]). What the link
  carries — the parent's whole table and its cuts — is
  [[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]'s.
- **A JSON-body 4xx is classified, not lost (X3).** `json_status_code` reads
  a JSON `"code": <code>` from the nested or flat error body, so an
  OpenRouter-shaped `{"code":400,…}` becomes a structured `Api` error rather
  than an opaque `Other`.

The hard rule: a new shape of committed message must pass through the same
admission step — never append an assistant message (or a tool result) the
serialiser would render in a form a supported provider rejects. This is the
exarch analogue of the wire-hop discipline
([[map/core/transport|exhaustive maps]]): the seam admits only validated,
complete values.

See also [[invariants/turn-ends-ready|exchange-ends-ready]] (the sequencing
half), [[map/exarch/agent|agent]] (`deliberate`, `admit_assistant`, `evict`),
[[map/exarch/provider|provider]] (`from_genai`, `json_status_code`).
