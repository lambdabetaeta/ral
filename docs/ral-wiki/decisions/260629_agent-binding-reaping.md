---
status: proposed
---

# Exarch leases agent scratch bindings; ral bindings do not expire

**A ral binding is lexical state; a lease is agent-host state.** A binding lives
until ral removes or shadows it. Exarch may lease an agent's top-level scratch
names and prune stale ones at ready boundaries. Reaping is a context and memory
policy of the embedding host, not a language rule.

## Context

An exarch run is a [[map/exarch/agent|fleet of agents]]. Each `Agent` owns one
persistent [[map/core/shell-state|`Shell`]]; the `Fleet` owns only shared
presentation and routing state such as the registry, bus, focus, and attachment
mode. The lease ledger is therefore per-agent, never fleet-global.

The shell's lexical store is `Mobile::scope`, an `Env` whose entries are
`Binding { value, scheme }`. That coupling is load-bearing: the runtime value
and the next turn's type seed are installed together. The same store also makes
excellent agent scratch, so a long run can accumulate forgotten names, old
closures, and settled handles with captured buffers.

The host must not manage that by opening `Env`. The
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
seam still holds: exarch supplies policy, core supplies behavioural operations,
and core owns the correspondence between a lease record and the live scope.

Three facts shape the design:

- **Lookups stay pure.** `Env::get` remains a lexical lookup. Reaping must not
  put atomics, clocks, or host callbacks on the evaluator hot path.
- **The domain is structural.** The reaper sees only the persistent top-level
  session scope above the prelude. Local scopes, closure captures, handlers,
  hooks, builtin tables, and dynamic context are outside the lease domain.
- **Pruning a name may not free its value.** `Lambda` and `Block` values carry
  captured `Arc<Env>` snapshots, and handles may be cloned or nested in other
  values. Reaping removes the future name and future type seed; physical memory
  follows only when no value still points at it.

## Decision

Exarch installs a **per-agent binding lease ledger** on that agent's `Shell`.

- **ral semantics are unchanged.** The REPL and ordinary ral hosts pass no lease
  epoch and observe no TTL. A ral program embedded in exarch observes only the
  ordinary consequence of a host-pruned name: the next lookup is undefined.
- **Only top-level lexical bindings are leased.** Prelude entries are excluded by
  scope. Baseline names seeded by boot/profile/agent library setup are excluded
  by the ledger. Aliases and handlers may get a separate policy later; they are
  not value bindings.
- **Lease metadata is not binding metadata.** `Binding { value, scheme }` stays a
  language object. A lease record lives in core-owned host scratch and is related
  to the live binding by `(name, generation)`.
- **The epoch is deterministic.** The first epoch is the agent's ral-tool-call
  generation:

  ```text
  type LeaseEpoch = u64
  ```

  Exarch increments it when that agent runs the `ral` tool. Core stores and
  compares the supplied epoch; core does not call a clock.
- **Use renews the lease.** Expiry is by idle ral-call age,
  `epoch - last_used`, not creation age.
- **Generation protects prune.** Rebinding a name creates a new generation. A
  prune deletes only the `(name, generation)` cohort it observed, so a stale
  prune cannot delete a newer binding.

## Ledger lifetime

The ledger lives on `Shell::local`, beside audit and other host-local scratch,
not on `Binding`, `Env`, or `Mobile`.

- **Not `Binding` or `Env`.** `Env` is lexical data and is captured by closures;
  lease state is host policy and must not travel with a captured scope.
- **Not `Mobile`.** Exarch stores a durable `MobileSnapshot` for panic recovery.
  If the ledger lived there, restoring a snapshot could roll back completed
  lease observations or leak lease records into a forked child.
- **Not `SessionState`.** The lease is host-local policy for this in-process
  embedding, not durable shell semantics, terminal authority, or source
  registry state.
- **Not inherited as live records.** `Shell::fork_session` snapshots the parent's
  lexical scope into the child agent, but the child starts with a fresh ledger.
  It inherits the parent's baseline names only; inherited non-baseline scratch
  receives the normal first-boundary grace in the child.

The core-owned shape is deliberately small:

```text
struct BindingLeases {
    baseline: HashSet<String>,
    records: HashMap<String, LeaseRecord>,
}

struct LeaseRecord {
    generation: u64,
    last_used: LeaseEpoch,
}
```

`baseline` is sealed after exarch finishes booting the agent shell: prelude,
profile/config scratch, and the agent library. `/clear` rebuilds the focused
agent's shell and seals a fresh baseline for that replacement shell.

## Behavioural seam

Core exposes lease behaviour, not the `Env` representation.

```text
Shell::seal_lease_baseline()
Shell::reconcile_leases(reads, writes, epoch)
Shell::prune_idle_leases(max_idle_calls, epoch, host_pins) -> Vec<PrunedBinding>
```

The normal host path may fold this into `Shell::run_source_turn`: `TurnRequest`
carries an optional lease epoch, and `TurnReport::Ran` can carry the committed
lease summary. With no epoch, no lease state changes. With an epoch, core
reconciles at the same commit boundary that installs the turn's post-run
`Mobile`.

Reconciliation is one correspondence check:

- A live top-level name with no record becomes a scratch record unless it is in
  `baseline`.
- A top-level write bumps the name's generation and resets `last_used`.
- A read renews `last_used`.
- A record whose name is no longer live is dropped; ral removed or shadowed it.

Every top-level write must pass through one lease-aware shell operation. Direct
session-scope `Env::set` / `set_binding` is not a valid install path for source
bindings, recursive groups, destructuring binds, module projections, plugin
loads, or host-seeded variables. The generation guard is sound only if every
rebind crosses the same cold path.

## Use tracking

The first implementation is **static and turn-scoped**.

- Core harvests a committed turn's read/write sets from the accepted typed IR
  and the command-head facts already used by dispatch.
- The read set includes every `Val::Variable` whose name is a live top-level
  session binding, including unchecked bindings whose scheme is `None`.
  Scheme instantiation alone is insufficient because unchecked names are
  deliberately skipped by `SessionSchemes`.
- The read set also includes command-position heads that resolve to lexical
  bindings, so `foo args` renews `foo` when `foo` is a bound block or lambda.
- The write set includes every persistent top-level install: name binds,
  recursive groups, destructuring components and rest names, module projections,
  and host seed/update operations.
- False-positive reads are acceptable; they keep scratch longer. False-negative
  reads cause surprising deletion and are bugs. Missed writes are ABA bugs.

Runtime lookup remains `Env` lookup. If a later feature creates dynamic uses
that static tracking cannot see, add a core-local per-turn touch collector at the
turn frame; do not add host callbacks to `Env::get`.

A failed parse or typecheck has no accepted read set and does not renew leases.
Core keeps no tombstones. Exarch records prune events so a later ordinary
`undefined variable` error has an explanation in the agent transcript.

Lexical capture is not chased. A closure that captured a value can keep using it
after the top-level name is pruned; that is lexical scope doing its job.

## Pinning

Pinning is a lease policy over names, not a new value kind.

- **Baseline names are pinned.** Boot/profile/library names are never lease
  candidates until `/clear` builds a new shell and baseline.
- **Live handles pin the name that reaches them.** Core walks the top-level
  binding's value through `List`, `Map`, and `Variant` payloads. If it finds a
  `Value::Handle` whose `HandleState` is still `Running`, that top-level name is
  pinned. Direct handles, aliases, and handles nested in records or lists all
  count.
- **The reaper does not settle handles.** A finished-but-unobserved handle may
  still read as `Running`; pinning it is the conservative choice. `poll`,
  `await`, `race`, boundary delivery, or a future registry can settle it through
  the normal handle path.
- **Settled and cancelled handles are scratch.** Once a handle is known
  `Completed` or `Cancelled`, its binding may be reaped by the same idle policy
  as any other value; cached stdout/stderr make it a good candidate.
- **Host pins are explicit.** A future durable-job or schedule registry may pass
  additional names or ids as `host_pins`. That is a host registry policy, not ral
  syntax and not a longer TTL on ordinary `spawn`.

This rule answers the aliasing hazard: if two top-level names point to the same
running handle, both remain. If a handle is reachable only through a closure
capture and no top-level value reaches it, the binding reaper has no name to
protect; worker cancellation remains governed by the handle's own scope, the
death-clock, and root abort.

## Reaping policy

Exarch reaps only at a ready boundary for that agent: after a `ral` tool call
settles and its tool result is committed, before the next provider request, or
at another point where no ral evaluation is running and no tool-result batch is
half-committed.

- **Idle expiry is call-count based.** Reap an unpinned binding when
  `last_used` trails the current agent ral-tool epoch by **256 ral calls**.
- **Fresh state gets one boundary.** A newly-created or just-used binding is
  protected for the boundary that observes it, so the model has one subsequent
  chance to name it.
- **Prune is committed state.** `prune_idle_leases` mutates the live top-level
  scope and returns `Vec<PrunedBinding>` naming what it removed.
- **The durable snapshot is refreshed immediately.** After a successful prune,
  exarch must replace the agent's durable `MobileSnapshot` with the shell's new
  `mobile_snapshot()` before any later panic recovery can restore the old
  bindings. This is part of the operation, not an optimisation.

No retained-size estimate is part of the first implementation. Size accounting
is hard to do honestly while closures and handles retain values behind `Arc`s,
and call-count expiry targets the pressure source: long agent sessions issuing
many ral tool turns.

No tombstones are part of the first implementation. The language diagnostic
stays ordinary; exarch records a compact prune event in its transcript/log.

## Ctrl-`\`, `/clear`, and durable work

The binding reaper is not a panic button.

- **Idle reap** removes stale names from one agent shell's top-level lexical
  scope and reports the removed names to exarch.
- **Ctrl-`\`** cancels the shell's durable root with `RootAbort`, reaping
  foreground work and detached workers. It does not delete lexical bindings.
  Bindings that hold aborted handles remain until ordinary lease policy prunes
  them.
- **`/clear`** rebuilds the focused agent's shell, clears its schedules and pin
  mirrors, cascades cancellation to its subtree, and seals a fresh lease baseline
  for the replacement shell. It is focused-agent state, not a fleet-wide reset.
- **Long-running durable jobs are separate.** A durable job is born into a
  host-managed, listable registry
  ([[decisions/260617_long-running-work|long-running-work]]). The registry may
  pin its handle name or expose cancel/list by id; do not encode durable work as
  a longer per-binding TTL. Avoid `disown` unless the REPL POSIX meaning is
  deliberately reconciled.

## Implementation parcels

1. **Core ledger and baseline.** Add `BindingLeases` to `LocalState`, the
   baseline sealer, and tests that REPL/bare hosts with no epoch never prune.
2. **Cold write door.** Route every persistent top-level install through one
   shell operation that bumps lease generation; make direct session-scope
   `Env::set_binding` unreachable from those paths.
3. **Static access harvest.** Return read/write sets for accepted source turns,
   including unchecked session names and lexical command heads.
4. **Recursive handle pin walk.** Classify top-level values for running handles
   without polling or settling them.
5. **Boundary prune in exarch.** After committed `ral` tool results, call prune,
   emit the compact prune event, and refresh `Agent::durable` immediately.
6. **Regression tests.** Pin false-negative reads, destructuring writes,
   recursive-group generation bumps, nested-handle pins, settled-handle pruning,
   stale-prune ABA protection, fork baseline inheritance, `/clear` baseline
   reset, and panic-recovery non-resurrection after prune.

## Consequences

- The model can use ral as scratch without growing an agent shell forever.
- The language remains simple: ral bindings do not expire; exarch scratch names
  may be reclaimed by a host policy.
- Core owns the correctness boundary: lease records, generations, static access
  harvest, and handle pinning all live beside the representation they interpret.
- Physical memory reclamation is best-effort until capture narrows. This removes
  future names and future type seeds, not every historical reference.

## Alternatives considered

- **Creation-age expiry.** Rejected: an old but active definition is not garbage.
  Use renews the lease, so expiry is by idle age.
- **Wall-clock expiry.** Deferred: time-based deletion is more surprising and
  harder to test. Call-count expiry matches the actual pressure source and can
  later accept a host-supplied clock if quiet multi-day sessions need it.
- **Explicit ral pin syntax.** Deferred: baseline, live-handle, and host-registry
  pins are enough for the first implementation. If users need durable scratch,
  add an exarch host command, not language syntax.
- **Retained-size eviction.** Deferred: retained size is not cheap or exact while
  closure snapshots and handles can keep values alive. Idle call-count reaping is
  useful without pretending to measure heap ownership.
- **Tombstones.** Deferred: they improve recovery diagnostics but are not part of
  correctness. The first version records prune events outside the language and
  leaves ordinary undefined-name diagnostics alone.
- **Lease fields on `Binding`.** Rejected: it mixes language state with host
  policy, makes closure snapshots carry lease state, and expands every binding
  for a policy only exarch observes.
- **Lease table on `Mobile`.** Rejected: `Mobile` is the panic-recovery snapshot
  and fork/session snapshot unit. Lease records belong to host-local state, and a
  prune must refresh the durable snapshot instead.
- **Runtime touch on every lookup.** Rejected initially: it taxes the hot path and
  entangles lexical lookup with host policy. Static per-turn sets are enough for
  the current language.
- **Make Ctrl-`\` delete bindings too.** Rejected: root abort is cancellation of
  running work, not session reset. `/clear` already names reset.
- **Let exarch inspect `Env` directly.** Rejected by
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]:
  the host receives operations, not representation.

## Open questions

- Should aliases eventually get a parallel lease policy over removable handler
  frames?
- Should prune events be model-visible tool context or only transcript/display
  facts?
- Should host pins name bindings, handle ids, or both once durable jobs land?

See also [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]],
[[decisions/260617_long-running-work|long-running-work]],
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]],
[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]],
[[map/core/shell-state|shell-state]], and [[map/exarch/agent|agent]].
