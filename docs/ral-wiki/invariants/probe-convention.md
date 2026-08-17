# The probe convention

**Every session-lived accumulator registers a probe: its name, current size,
cap, and pressure policy. An accumulator without a probe is a review
defect.** The rule turns
leases-and-budgets' prose principle —
every session-lived thing must say what bounds it — into a checkable
convention: a budget that cannot be inspected will be debugged by restarting
the process.

A probe is a `ProbeRow` (`exarch/src/agent/resources.rs`): `name`, `current`,
`cap: Option<u64>`, a `policy` from the closed pressure vocabulary
(`coalesce` / `reject` / `evict` / `reap` / `warn` / `none (unbounded)`),
and an optional note. Multi-figure accumulators emit one row per figure
(`viewport.blocks`, `inbox[user]`, …). The inspector deliberately precedes
the enforcer: an accumulator whose cap lands later states its *decided*
policy with `cap: None` and a note saying so — never a fake cap, never
silence.

`/resources` is a **fold over the registered probes**, not a bespoke report.
It has two halves, split by who may legally read what: the agent assembles
its own rows on its attend thread (the shell's worker registry and bindings,
its inbox, model projection, record log, and disk) and emits them as one
`Transient::Resources` —
raw rows beside the rendered card — and the frontend appends the rows for the
accumulators *it* owns (viewports, views, the bus) at render time. Neither
half reaches across a thread for the other's figures, and the fold is never
model-facing. A probe fold is an interactive diagnostic, read when it is
run: no session keeps a pressure history, so the figures live only in the
fold's own emission and never in `record.jsonl`.

Two laws bound what a probe may do:

- **Probing never mutates and never renews.** Enumeration is not
  observation — the same rule that keeps the `services` pin's reconciliation
  from renewing a service's own state. If probing renewed, the operator's own
  diagnostics would immortalise every zombie they exist to reveal.
- **Residents and accumulators are both probed; only residents are listed,
  cancelled, and leased.** The line is the capability
  ([[decisions/260705_session-ledger|session-ledger]]): a resident (a
  worker, an agent, a schedule) can be reached and controlled by id; a mere
  accumulator (a viewport, the bus, an inbox) can only be measured and
  bounded. A probe row is the one facet the two kinds share.

The hard rule for new code: a session-lived accumulator lands together with
its probe row in the `/resources` fold, and its row's policy names how the
accumulator behaves under pressure — even when the enforcement ships later.

See also [[decisions/260705_session-ledger|session-ledger]] (the
resident/accumulator line; the probe as one of a resident's four facets),
[[map/exarch/agent|agent]] (the agent half of the fold),
[[map/exarch/builtins|builtins]] (the `services` pin, the same
enumeration-is-not-observation law — `workers` itself was since retired as a
model-facing listing, legibility now split by lease class).
