---
status: accepted
---

# Context pressure warns the model through the nudge registry

**Auto-compaction fires at deliberation entry and the model hears about it
only afterwards, as a summary where its history used to be. This ADR gives it
a warning: a soft pressure line one full reserve ahead of the compaction
trigger, delivered as a budget-free part in the nudge registry's
clean-completion composition — the same channel and accounting as the
pinned-state reminder — latched to fire once per crossing and re-armed by the
compaction itself. The `/resources` probe row is corrected in the same
stroke: on a known-window model it now shows tokens against the trigger that
actually fires, not bytes against the unknown-window fallback.**

## Context

`Agent::compact` runs once, at deliberation entry — the sole boundary
guaranteed `ReadyForUser` — and decides on real context pressure: the tokens
the model last saw (`last_input`, refreshed every step) against the window's
reserve (`compaction_due`), falling back to serialized bytes against
`COMPACT_THRESHOLD` when the window is unknown. Nothing pushed that pressure
to the model before it fired. The only visibility was pull — the `/resources`
probe — and its `log.bytes` row reported bytes against the fallback threshold
even when the live trigger was tokens against the window, so on any
known-window model the pressure the model could see was not the pressure that
fires.

Everything the warning needs had grown in independently: the nudge registry
composes budget-free reminder parts into one `EXARCH_REMINDER` self-post on
clean completion; `last_input` is already the trigger's numerator; and
[[decisions/260630_long-session-resource-budgets|long-session-resource-budgets]]
(carried forward by [[design/residency|residency]]) already states the policy
the warning should preach: prefer paths over captured bytes.

## Decision

- **A soft line, one full reserve early.** `pressure_due(used, window)` fires
  when `used + 2·reserve > window` — the compaction trigger less one reserve
  (~30k tokens on a 200k window), room enough to act. Unknown windows use the
  byte fallback at three-quarters of `COMPACT_THRESHOLD`. Pure functions in
  `digest`, beside `compaction_due`.

- **Delivered as a nudge part, not a rule.** The registry's `RULES` classify
  attempt outcomes; pressure is not a property of the outcome, so it joins
  the clean-completion `parts` composition instead, exactly as the
  pinned-state reminder does: budget-free, additive with the reply and pin
  parts, recorded through `record_nudge`. Unlike the pin reminder it is not
  gated on live children — pressure is the agent's own and cannot wait.

- **Latched per crossing.** `context_warn_latched` on the `Agent`, sibling to
  `disk_warn_latched`: set when the warning is delivered, cleared by a
  successful compaction and by reset, so the model is told once per
  excursion, not on every completion under pressure.

- **The sermon prefers paths.** The warning names the numbers, says the older
  history will be summarized soon, and directs salvage outward: write durable
  state to files and keep the paths — not large values in bindings — and
  record intent with `set-goal`/`add-task`.

- **The probe row tells the truth.** With a known window, `/resources` emits
  `context.tokens` — `last_input` against `compaction_trigger(window)` — and
  `log.bytes` becomes an uncapped gauge; the byte cap survives only in the
  unknown-window arm, where it really is the trigger.

## Alternatives considered

- **Steering injection at the tool boundary inside `deliberate`.** Strictly
  better timing: pressure builds mid-deliberation where tool boundaries are
  plentiful, the warning would cost no extra round-trip, and it would always
  precede the entry compaction — the nudge path can lose that race when one
  tool-heavy deliberation crosses both the soft and the hard line, so the
  model reads the warning only after the summary. Rejected on channel
  morality: steering is user ingress by contract, and the registry is the one
  system-to-model voice, with the brackets, the budget bookkeeping, and the
  forensic trail. The race's failure mode is a stale warning, not a lost one,
  and the soft margin (a full reserve) makes it rare.

- **A `RULES` entry.** Rejected: a `Rule` is a function of the attempt
  outcome, and pressure is not derivable from one.

## Consequences

- The crossing costs one extra provider round-trip — the nudge turn — which
  is precisely the salvage turn the warning exists to buy.
- After a tool-storm deliberation the warning can arrive post-compaction and
  read stale. Accepted; see the steering alternative.
- `/resources` now shows the gauge that fires, and the model can watch its own
  pressure between warnings.

## See also

[[decisions/260630_long-session-resource-budgets|long-session-resource-budgets]]
(the paths-over-bytes policy the warning text enforces),
[[design/residency|residency]] and
[[decisions/260705_session-ledger|session-ledger]] (the accounting regime the
probe row belongs to), [[invariants/probe-convention|probe-convention]]
(`/resources` as a fold over budget probes), and [[map/exarch/agent|agent]]
(the nudge registry and the attend loop that owns the latch).
