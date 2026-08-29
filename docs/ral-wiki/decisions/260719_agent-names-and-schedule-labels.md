---
status: active
---

# One `agent` verb by record; names and schedule labels are identity; commitments retired

**The agent surface collapses to a single record-spec `agent` verb, and the
model addresses what it started by the *name* it gave, not a numeric id — while
the protected-commitment machinery is deleted whole.** Three changes land
together in one pass (`ca26748`): the two spawn verbs become one closed-record
builtin, the model-facing identity of an agent becomes its name and of a
schedule its label, and the `commit`/`verify-commitment` feature is removed
root and branch. Each is a distinct decision; they share a fork.

## One spawn verb, a closed record

`amnemon` and `mnemon` are no longer *verbs*. There is one builtin

```text
agent [prompt: Str, name: Str, type: `amnemon|`mnemon, grant: <one of six>] → F [name: Str, log-dir: Str]
```

and `amnemon`/`mnemon` survive only as the two **memory modes** the `type`
field selects ([[decisions/260702_subagent-memory-modes|subagent-memory-modes]]'s
tabula-rasa/inherits split is untouched — it moves from the verb to a field).

- **The record row is closed; the two variant rows inside it are open.** A
  record literal with literal keys infers an exact row, so unifying it against
  the closed spec row makes a missing or misspelled field a *static* error
  naming that field. `type` and `grant`, by contrast, hold open variant rows: a
  literal tag infers its own open row, so an unknown tag reaches the runtime
  door ([`agent_type_label`]/[`permission_label`] in
  `exarch/src/shell_eval/builtins/harness.rs`) that enumerates the legal labels,
  rather than dying as a bare row-unification mismatch. "Closed rule, named
  labels" lives at the door for exactly the two arguments whose vocabulary the
  model must be *told*. This is the same shape [[map/exarch/builtins|`schedule`]]
  already carries for its own spec record.
- **The record is extensible where positional arguments were not.** A future
  per-child budget, a memory-window cap, a model override — each rides as a new
  optional field on the spec, decoded at the same door, without a new verb or a
  positional-arity break. The surface is open toward the budgets
  leases-and-budgets leaves for later.

## Names are the model-facing identity

An agent's **name** is unique among live agents fleet-wide and is the only
handle the model ever holds: `message <name>`, `agent-cancel <name>`, and
`agents` → `[[name, elapsed-s, log-dir]]`. Numeric ids leave the model surface
entirely; the internal `AgentId` plumbing (the registry tree, the cascade, the
inbox routing) is unchanged beneath it.

- **A name survives what an id does not.** It reads in prose, so a plan the
  model wrote to itself still names its children after a context compaction; and
  `agents` re-lists the live names to recover them regardless. An opaque integer
  carried none of this.
- **A name closes the silent cross-family confusion.** When an agent was an id
  and a schedule was an id, `unschedule <agent-id>` was a well-typed *silent
  no-op* — the wrong integer in the right hole. Names and labels are disjoint
  string namespaces, so a mistargeted call now fails to resolve loudly instead
  of doing nothing.
- **Uniqueness is enforced twice.** The desk's `launch` pre-check refuses a
  duplicate name didactically before ever forking a nursery session
  (`AgentRegistry::name_live`); `AgentRegistry::register` closes the race
  authoritatively under the registry lock (`RegisterError::NameTaken`), so two
  same-name spawns within one turn cannot both succeed. The cheap didactic half
  and the race-free half are the same rule at two costs.

## Schedule labels are the schedule identity

Symmetrically, a schedule is addressed by its **label**. `schedule` answers a
receipt `[label: Str, next-s: Int]`; `unschedule <label>` removes by label;
`schedules` lists `[[label, trigger, next-s, fires]]`. A label is unique among
live schedules, and the `sched-<n>` form is reserved for minted defaults, so a
user label can never coincide with one by construction.

- **The receipt's `next-s` catches a mis-meant time at arm time.** A cron
  expression that parses but does not *mean* what the model intended is caught
  by reading back the seconds to first fire — the receipt turns a silent
  scheduling error into an immediate one, the same discipline names give the
  spawn call.

## Amendment (2026-08-06): the label is required, and mints nothing

The `sched-<n>` default this section describes is retired. Once the desk's
per-call recording unified onto one `subject` field feeding both the rail row
and the audit sentence, an unlabelled schedule left that one field with two
possible answers — the caller's (none) and the registry's (the minted
default) — and the two could disagree. The repository owner ruled that
`schedule` must always take a label from the model, closing the question at
its source: [[decisions/260720_harness-calls-are-acts|harness-calls-are-acts]]
carries the amendment in full. `sched-<n>` is no longer a reserved shape —
there is nothing left for it to protect.

## Commitments retired

The protected-commitment feature is deleted whole: the `commit` and
`verify-commitment` builtins, the `commit-open`/`commit-verify` desk arms, the
`CommitmentIntent`/`CommitmentSettle` classes, the `commitment:*` protected-pin
projection, and `PinKind`. It did not earn its machinery — a host-owned
writer/verifier orchestration, a reserved keyspace, a typed pin kind, and a
bespoke nudge — for the discipline it bought; the operator judged the feature a
mistake.

- **The pin register survives as the plain digest mirror.** A `PinDigest` map
  still mirrors `tasks`/`goal` state for the nudge ([[design/pins|pins]]), and
  the single host-owned **`services` ledger pin** remains write-protected —
  ordinary `surface` calls cannot write or clear the `services` key
  (`reject_protected_pin`, `exarch/src/shell_eval.rs`), because the host
  maintains it as services are born and settle. That one protected slot is all
  that remains of the reserved-keyspace idea; there is no longer a *kind* on a
  pin.

  > **Amended 2026-08-27.** The `services` protection recorded above is
  > removed. `SERVICES_PIN_KEY`, `reject_protected_pin`, the
  > `accepted_surface` wrapper that carried it, `Avatar::reconcile_service_pins`
  > (the host writer that kept the pin in sync with live durable workers), and
  > `services_pin_card` are all deleted — the operator's own words: "kill the
  > protected pins, that mechanism shouldn't exist at all," no deeper
  > technical rationale offered or needed. The pin register is now entirely
  > model-authored: no reserved key, no host-only writer, nothing a program is
  > refused. Durable-service legibility through the register is gone and not
  > replaced by another pin; what remains is the same worker-birth trail card
  > an ordinary `spawn` gets (`worker #id cmd durable`) plus an aggregate
  > running count on `/resources` — see [[design/pins|pins]] and
  > [[map/exarch/builtins|builtins]].

## Consequences

- The spawn, listing, message, cancel, and schedule builtins all take or return
  **ral records** the model can bind, filter, and fan out over — the receipt is
  the value, not a stringly-typed line it re-parses.
- [[decisions/260703_protected-commitment-pins|protected-commitment-pins]] is
  superseded; its design no longer stands.
- [[decisions/260702_subagent-memory-modes|subagent-memory-modes]] stands: the
  two memory contracts are exactly the `type` field's two tags.
- [[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
  stands: the spawn family is still a ral builtin answered by `ExarchDesk`
  over the [[design/engine-protocol|engine protocol]]'s enquiry rail; only the class
  inventory shifts (the commitment classes leave, the spawn enquiry carries
  the name).
- [[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]] stands: fuel still
  bounds delegation depth; the retired commitment builtins simply leave the set
  of verbs a spent-fuel agent cannot reach.

## See also

[[design/agents|agents]] (the timeless surface),
[[design/pins|pins]] (the register that outlives commitments),
[[map/exarch/builtins|builtins]] (the verbs as they stand),
[[map/exarch/tools|tools]],
[[decisions/260703_protected-commitment-pins|protected-commitment-pins]] (superseded here),
[[decisions/260702_subagent-memory-modes|subagent-memory-modes]] (the memory modes, now a field),
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]] (the builtin migration this rides on),
[[decisions/260617_async-agent-tool|async-agent-tool]] (the launch-only async edge kept),
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]] (fuel bounds depth),
[[decisions/260705_branch-minimal|branch-minimal]] (the conversing mnemon child),
[[design/engine-protocol|engine-protocol]] (the rail the builtins answer over).
