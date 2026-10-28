# The transcript admits only sendable messages

Every message committed to a session's
[[internals/session-record|record-backed model projection]] serialises to a
request **every supported provider accepts.** That projection is the single
source of truth for the next request; once a malformed or empty message is
committed, it is re-serialised on every subsequent round-trip, so a
single bad commit can wedge the whole run on a strict backend (Anthropic 400s,
the run dies — misclassified as fatal). The protocol state machine
([[internals/session-record|`record/session.rs`]]) enforces *sequencing* — above all that a
`tool_use` block is answered; this invariant enforces *per-message
admissibility* at the one place messages enter the log: the `deliberate` commit
boundary in [[map/exarch/agent|agent]].

Sequencing itself is judged by the very function that folds. `Context::step`
judges a record before applying it, so a hand-edited or foreign file is refused
by the same rule a live session is held to; there is no second fold to
disagree with the first, and nothing for a resume to compare
([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]).

Sequencing is **not** strict user/assistant alternation, and never was.
Consecutive same-role messages are routine: a session that has evicted sends
one **residue** — a bracketed user-voice message carrying the eviction's note —
where each evicted run began, between items and never inside a turn, so a
residue adjacent to a prompt puts two user-voice messages in a row, whether
it stands before the oldest surviving prompt or mid-history
([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]). A
residue is never the last message while a request is built: the unclosed
turn is never in a cut, and a forced eviction never names the newest item.
Its user voice is also what makes a prompt's eviction
legal to the provider: the frontier opens with a user-role item whether its
first prompt stayed or left. Likewise an inherited turn can end on a user
message with the child's launch prompt behind it, and a prompt whose predecessor's answers were interrupted follows
those answers' last tool results or steering directly
([[invariants/turn-ends-ready|exchange-ends-ready]]). genai's Anthropic
adapter pushes one JSON object per `ChatMessage` and merges nothing, so those
reach the provider as written. What the wire actually constrains is the
tool-call *pairing* — a structural relation between content blocks — which is
why `AwaitingToolResults` and `validate_result_ids` exist, and why a quiesce
answers every call that never ran before the next prompt is admitted: a
dangling `tool_use` must never reach the wire.

**Admission requires an available durable record, not merely a ready
`Context`.** Quiesce advances that context only after each owed repair record
commits. A mandatory repair that cannot commit, like any failed append,
latches the writer and ends the session's life, so nothing more is drawn.
The owner announces the failure once at its end. Explicit resume after
storage repair replays what really committed and answers only the tool ids
still outstanding; it never inserts an unrecorded result to make a live
session look sendable.

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

- **Eviction runs where it can (X1).** It needs only `Context::can_evict`
  (`is_ready` → `admits_new_turn`) — false solely at `AwaitingToolResults`,
  strictly weaker than `ReadyForUser`: an edit may land at any rest but a
  batch in flight, because outstanding tool calls name the assistant frame
  their results answer and nothing may come between. This is why the harness
  weighs the context at **every turn boundary**, not at `deliberate` entry
  ([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]). What keeps
  a cut off the work in hand is not this predicate but the rule that **no edit
  touches the unclosed turn** — the one being written, which exists only while
  a turn is open: `Context::resolve_cut` refuses a run with it within, and
  `plan_forced_eviction` walks back from the newest-but-one frontier item, so a
  forced eviction cannot even name it. The plan is one prefix span and may take
  the live prompt with the rest; the model, warned first, may evict the
  prompt under its own note
  ([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]).
- **`Inherited` stands only at a fork's opening.** `Context::step` refuses
  `Protocol::Inherited` anywhere but the first protocol record of a child log —
  the position *is* the rule, `len == 0`, needing no flag and no reading of
  the table. The record is
  sequencing-neutral — it commits no message and advances no state — so the
  rule cannot be read off a `State` alone; it is the fold's own, and it is
  what makes "a log has at most one ancestry, fixed at the fork that opened it"
  a property of the file rather than a habit of the writer
  ([[decisions/260906_context-rollover|context-rollover]]). What the link
  carries — the parent's whole table and its residues — is
  [[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]'s and
  [[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]'s, the
  judge refusing residues that are not laminar or rows whose `location`
  disagrees with them. It is also
  the one record whose meaning lies in another file: its step reads each
  resident row's records back through digest-checked pointers, keeping each
  row's admissible prefix so that no dangling `tool_use` of the parent's
  spawning batch reaches the child's wire, and an unreadable parent refuses
  the record before the table changes
  ([[internals/session-record|session-record]]).
- **`System` and `Cleared` are edits, not messages.** `Protocol::System` sets the
  prompt every later request goes out under and, like `Inherited`, is
  sequencing-neutral; it never enters the transcript, the prompt travelling in
  `Request.system`. `Protocol::Cleared` withdraws every turn and, like
  `Rewound`, is admitted whenever a new turn is, so it too is refused only
  while tool results are owed.
- **A harness-authored turn is an `Import`.** `Protocol::Import { turn, message }`
  always opens a fresh turn under a newly minted id, and a stale one is refused;
  it carries no inherited material.
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
