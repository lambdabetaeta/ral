# An exchange ends ready

The session protocol is a state machine over the
[[map/exarch/frontend|event log]] (`AgentLog`) with two kinds of phase:

- `ReadyForUser` admits a fresh prompt;
- the intermediate phases (`AwaitingAssistantAfterUser`, `AwaitingToolResults`,
  `AwaitingAssistantAfterToolResults`) carry an exchange in flight.

The attend loop reads the next prompt only in `ReadyForUser` —
`AgentLog::append_user` rejects a prompt in any other phase, since committing
one mid-exchange would break the user/assistant/tool role-alternation the
provider request depends on.

The invariant that keeps the loop sound: **`Agent::take_up` never hands
control back to `Agent::attend`'s loop until the session is `ReadyForUser`,
however the exchange ended** — a clean reply, a user cancellation, the step
cap, or a surfaced provider error. `Agent::deliberate` commits a prompt (or a
tool-result batch) before the round-trip it drives, so a failure or a capped
step count between that commit and the next assistant reply strands the
machine in an `AwaitingAssistant*` phase with no reply recorded; left there it
rejects every later prompt and the session is wedged. Two of `deliberate`'s
three unclean exits close themselves — `replied`/`cancelled` call
`AgentLog::quiesce` with `QuiesceReason::Replied`/`Cancelled` — but a
step-capped or transport-failed round-trip returns still mid-protocol, so
`take_up` calls `quiesce(QuiesceReason::Aborted)` immediately afterward
whenever `is_ready` does not already hold, whether `deliberate` returned
cleanly or was caught out of a panic. `Agent::attend` (and its bounded twin
`attend_backlog`) repeats the same check with a `debug_assert` once its own
loop exits — a backstop against a future exit path that bypasses `take_up`.

The hard rule: a path that ends an exchange must leave the session
`ReadyForUser`. Add a new exchange-ending outcome through `quiesce` (extend
`QuiesceReason`), never by returning while a prompt sits unanswered
mid-protocol. `is_ready` is the single predicate for "a fresh prompt is
admissible"; compaction reads it too ([[map/exarch/agent|agent]] `can_compact`).

See also [[map/exarch/frontend|frontend]] (the state machine and `AgentLog`),
[[map/exarch/agent|agent]] (`attend`, `take_up`, `deliberate`).
