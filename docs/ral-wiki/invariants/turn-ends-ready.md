# An exchange ends ready

The session protocol is a state machine over the
[[internals/session-record|record-backed model projection]] (`AgentLog`) with
two kinds of phase:

- `ReadyForUser` — the exchange is settled;
- the intermediate phases (`AwaitingAssistantAfterUser`, `AwaitingToolResults`,
  `AwaitingAssistantAfterToolResults`) carry an exchange in flight.

**`is_ready` is the single predicate for "a fresh prompt is admissible", and it
is weaker than `ReadyForUser`.** A record that opens a span — a
`Protocol::UserPrompt`, an imported `Protocol::ContextMessage` — is admissible
in every phase but `AwaitingToolResults`: an exchange the model never replied to
is *abandoned* by the next prompt, not closed by a fabricated one. Only
outstanding tool calls hold the log, because their answer is genuinely owed and
a dangling tool-call block is not a legal request
([[invariants/transcript-admission|transcript-admission]]). `admissible` in
`record/model.rs` and `is_ready` read the same rule, so what a live door accepts
and what a replayed log admits cannot drift.

What the phases sequence is *tool-call pairing*, not strict user/assistant
alternation — which this projection has never maintained and does not aim to.
Consecutive same-role messages are routine and reach the provider as written
([[invariants/transcript-admission|transcript-admission]]); an unanswered
`tool_use` block is the shape that is actually illegal, and it is the one the
phases exist to prevent.

The invariant that keeps the loop sound: **`Agent::take_up` never hands control
back to `Agent::attend`'s loop until a fresh prompt is admissible, however the
exchange ended** — a clean reply, a user cancellation, the step cap, or a
surfaced provider error. `Agent::deliberate` commits a prompt (or a tool-result
batch) before the round-trip it drives, so a failure or a capped step count
between that commit and the next assistant reply leaves the machine in an
`AwaitingAssistant*` phase with no reply recorded. That phase is now a legal
resting place, and costs nothing: `replied`/`cancelled` call `AgentLog::quiesce`
with `QuiesceReason::Replied`/`Cancelled`, `take_up` calls
`quiesce(QuiesceReason::Aborted)` whenever `is_ready` does not already hold —
whether `deliberate` returned cleanly or was caught out of a panic — and
`Agent::attend` (with its bounded twin `attend_backlog`) repeats the check with
a `debug_assert` once its own loop exits, a backstop against a future exit path
that bypasses `take_up`.

`quiesce` records only what the log still owes, and never a turn the model did
not take:

- an answer to tool calls that never ran, whatever ended the exchange — the
  calls were really made, and "not executed" is really the answer;
- a capstone for an exchange that ended on `reply`, because the fold cannot
  otherwise tell a reply from an interruption at the resting phase the two
  share, and would drop the child's whole turn from its own view.

Nothing else is synthesised, and an abandoned exchange is left exactly as it
lies. `record.jsonl` keeps it unabridged for the TUI, resume, and the human
audit trail; the model reads none of its content. A *closed* span whose own
fold does not settle renders as exactly one `User`-role note in place of
everything it held — which catches by the same test the exchange abandoned
with pending tool ids, whose dangling tool-call block must never reach the
wire. The note is cause-neutral, a cancel and an abort being told apart only
by a `Forensic` record this fold never sees, and says whether tools had been
called: their effects outlive the context the exchange lost, so a model that
reads no trace of them would re-run them.

Its voice is the point. The harness may state a fact about the conversation;
it must never put words in the model's mouth. The
placeholder this replaced (`"[EXARCH // Request interrupted by user.]"`) rode
forward into the model's own history on every later round-trip, and in one
recorded session the model — having seen itself apparently say
`(cancelled by user)` a few turns earlier — emitted that string verbatim as a
genuine `stop_reason: "completed"` reply. A `System`-role marker in its place
is not the fix either: genai's `ChatRequest::iter_systems`/`join_systems` hoist
every system message anywhere in history into one preamble resent on every
request, so a marker meant to stay pinned where it happened would colonise the
system prompt instead.

`is_live_exchange` — the exchange a context edit may not name — stays keyed to
`ReadyForUser` rather than `is_ready`, so an edit can never land on an exchange
a deliberation is still driving.

The hard rule: a path that ends an exchange must leave a fresh prompt
admissible. Add a new exchange-ending outcome through `quiesce` (extend
`QuiesceReason`), never by returning with tool calls unanswered. Compaction
reads `is_ready` too ([[map/exarch/agent|agent]] `can_compact`).

See also [[internals/session-record|session-record]] (the durable protocol and
its model fold),
[[map/exarch/agent|agent]] (`attend`, `take_up`, `deliberate`).
