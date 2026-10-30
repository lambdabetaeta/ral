# An exchange ends ready or explicitly unavailable

An **exchange** takes up one input through its nudge continuations; a job
is the recorded busy period that may contain several exchanges. Neither is
a unit of context, whose atoms are turns
([[decisions/261029_an-agent-is-a-thread|an-agent-is-a-thread]],
[[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]).

The session protocol is a state machine over the
[[internals/session-record|record-backed model projection]] (`AgentLog`) with
two kinds of phase:

- `ReadyForUser` — the exchange is settled;
- the intermediate phases (`AwaitingAssistantAfterUser`, `AwaitingToolResults`,
  `AwaitingAssistantAfterToolResults`) carry an exchange in flight.

**`is_ready` is the single predicate for "a fresh prompt is admissible", and it
is weaker than `ReadyForUser`.** A record that opens a turn — a
`Transcript::UserPrompt`, an imported `Transcript::Import` — is admissible
in every phase but `AwaitingToolResults`: an exchange the model never replied to
is *abandoned* by the next prompt, not closed by a fabricated one. Only
outstanding tool calls hold the log, because their answer is genuinely owed and
a dangling tool-call block is not a legal request
([[invariants/transcript-admission|transcript-admission]]). `admissible` and
`is_ready`, both in `record/context/state.rs`, read the same rule, so what a live
door accepts and what a replayed log admits cannot drift.

What the phases sequence is *tool-call pairing*, not strict user/assistant
alternation — which this projection has never maintained and does not aim to.
Consecutive same-role messages are routine and reach the provider as written
([[invariants/transcript-admission|transcript-admission]]); an unanswered
`tool_use` block is the shape that is actually illegal, and it is the one the
phases exist to prevent.

The owner boundary has two outcomes: **a fresh prompt is admissible and the
session is usable, or recording has failed and the session is explicitly
unavailable.** A clean reply, user cancellation or provider error normally
leaves the former. A failed mandatory repair cannot: `Context` remains the
fold of committed records, never an invented ready state. The writer latches
that failure and ends the owner's life; the owner's terminus announces it
once and makes it the outcome. Other sessions remain independent.
`Agent::deliberate` commits a prompt (or a tool-result batch) before the
round-trip it drives, so a failure between that commit and the next assistant
reply leaves the machine in an `AwaitingAssistant*` phase with no reply
recorded. That phase is a legal resting place. `replied`/`halted` call
`AgentLog::quiesce` with `QuiesceReason::Replied`/`Cancelled`.
`deliberate` repairs on every return with `quiesce(QuiesceReason::Aborted)`
when needed; `attempt` repairs after an unwind. A recording failure ends
life; no subsequent provider work starts from an unavailable session.

`quiesce` records only what the log still owes, and never a turn the model did
not take:

- an answer to tool calls that never ran, whatever ended the exchange — the
  calls were really made, and "not executed" is really the answer;
- a capstone for an exchange that ended on `reply`, because the fold cannot
  otherwise tell a reply from an interruption at the resting phase the two
  share, and would drop the child's whole exchange from its own context.

Nothing else is synthesised, and an abandoned exchange is left exactly as it
lies — in `record.jsonl` and in the context alike. `Context::rendered` sends
every resident turn as written; the next prompt, following the interrupted
exchange's last tool results, is the interruption's own mark. The unrun-call
answer above is what keeps this legal: a closed exchange can never hold a
dangling tool-call block, because exchange-boundary repair precedes the next
prompt.

The content must stay because an exchange includes *all the turns driven by
its input*. One recorded session had a single exchange run fifty-seven
tool turns; an Esc there, followed by "what are you struggling with", left the
model — its whole session replaced by a one-line "its content is not in your
context" note — running `git status` and then reading its own `record.jsonl`
to find out what it had been doing. Dropping an interrupted exchange is not
tidiness, it is amnesia.

What the harness may not do is speak for the model. The placeholder before
that (`"[EXARCH // Request interrupted by user.]"`, an *assistant* capstone)
rode forward into the model's own history on every later round-trip, and in one
recorded session the model — having seen itself apparently say
`(cancelled by user)` a few turns earlier — emitted that string verbatim as a
genuine `stop_reason: "completed"` reply. A `System`-role marker in its place
is not the fix either: genai's `ChatRequest::iter_systems`/`join_systems` hoist
every system message anywhere in history into one preamble resent on every
request, so a marker meant to stay pinned where it happened would colonise the
system prompt instead.

Eviction is not stated at this granularity at all. The rule is that **no
edit touches the unclosed turn** — the one being written, which exists only
while a turn is open, so at `ReadyForUser` nothing is unclosed
and a user rewind or clear (`Transcript::Rewound` and `Transcript::Cleared`, withdrawals
rather than cuts, admitted whenever a new turn is) may empty the context. `Context::resolve_cut` refuses a
run with that turn within, and `plan_forced_eviction` walks back from the
newest-but-one frontier item, so a forced eviction cannot name it either
([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]). The reading door
refuses the same turn, and that refusal names the closed turns of the work in
hand, which are readable
([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]).

The hard rule: a usable session ends an exchange prompt-admissible. Add a
new exchange-ending outcome through fallible `quiesce` (extend
`QuiesceReason`), never by pretending an uncommitted tool result exists.
Every append that does not commit — failed write, unencodable record or
poisoned append lock — and any durable record the fold refuses latches the
shared writer once, whichever producer meets it first, and ends the life
bound to it. After storage is repaired, explicit resume reconstructs the
durable prefix and commits any repair still owed before admitting work.
No automatic retry or alternate in-memory log
supplies an unavailable session's authority. Eviction reads `is_ready` too
([[map/exarch/agent|agent]] `can_evict`).

See also [[internals/session-record|session-record]] (the durable protocol and
its model fold),
[[map/exarch/agent|agent]] (`run`, `admit`, `deliberate`).
