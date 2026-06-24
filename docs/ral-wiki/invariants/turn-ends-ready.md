# A turn ends ready

The session protocol is a state machine over the
[[map/exarch/frontend|event log]] with two kinds of phase:

- `ReadyForUser` admits a fresh prompt;
- the intermediate phases (`AwaitingAssistantAfterUser`, `AwaitingToolResults`,
  `AwaitingAssistantAfterToolResults`) carry a turn in flight.

The driver reads the next prompt only in `ReadyForUser` — `SessionLog::append_user`
rejects a prompt in any other phase, since committing one mid-turn would break the
user/assistant/tool role-alternation the provider request depends on.

The invariant that keeps the driver sound: **`run_turn` returns the session in
`ReadyForUser`, however the turn ends** — natural completion, user cancellation,
or a surfaced provider error. A turn commits its prompt before the request is
sent, so a failure between commit and reply strands the machine in an
`AwaitingAssistant*` phase with no reply recorded; left there it rejects every
later prompt and the session is wedged. Every exit of `run_turn` therefore passes
through a single point that calls `SessionLog::quiesce` unless `is_ready` already
holds — synthesising the tool-result and assistant stubs role-alternation needs to
wind the machine back — and a `debug_assert` on that post-condition catches any
future turn-ending path that forgets.

The hard rule: a path that ends a turn must leave the session `ReadyForUser`. Add
a new turn-ending outcome through `quiesce` (extend `QuiesceReason`), never by
returning while a prompt sits unanswered mid-protocol. `is_ready` is the single
predicate for "a fresh prompt is admissible"; compaction reads it too
([[map/exarch/agent|agent]] `can_compact`).

See also [[map/exarch/frontend|frontend]] (the state machine and `SessionLog`),
[[map/exarch/agent|agent]] (`run_turn`, the turn driver).
