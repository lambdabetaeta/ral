---
status: proposed
---

# Exarch leases scratch bindings; ral bindings do not expire

**A ral binding is language state; a lease is host state.** A ral binding lives
until the program removes or shadows it. Exarch may lease a top-level binding as
host scratch and reap that session name at safe boundaries. Reaping is a
memory/context policy, not a language rule. Ctrl-`\` kills running work;
`/clear` resets the shell; the binding reaper only removes stale top-level
scratch names.

## Context

`Session` owns a persistent [[map/core/shell-state|Shell]]. Its mobile scope is
an `Env`; a scope entry is `Binding { value, scheme }`, so the next turn's type
seed and the runtime value travel together. That state is useful scratch for an
agent, but it can also accumulate forgotten names and settled handles with
captured buffers.

The host must not manage that by opening `Env` itself. The
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
seam still holds: exarch supplies the policy, and core supplies behavioural
accessors. Core owns the representation and the generation guard.

Three facts shape the design:

- **Lookups stay cheap.** `Env::get` is on the evaluator hot path. Reaping must
  not put atomics, clocks, or host callbacks on every variable lookup.
- **The leased scope is structural.** Prelude loading leaves a distinct session
  top-level scope at a ready boundary. The reaping domain is that scope, named
  through core accessors, not a fold over every lexical frame.
- **Pruning a binding may not free its value.** `Lambda` and `Block` values
  carry captured `Arc<Env>` snapshots, and live workers can also retain values.
  Reaping removes the session name and shrinks future seeds; physical memory
  follows only when no capture still points at the value.

## Decision

Exarch installs a **binding lease ledger** over the session top-level lexical
scope.

- **ral semantics are unchanged.** The REPL has no binding TTL. A ral program
  does not observe expiry except when embedded in exarch's host policy. Prelude
  bindings, rc/profile bindings, exarch's boot library, and live top-level
  handles are never reaped.
- **Only top-level session bindings are leased.** Local scopes, closure-captured
  scopes, prelude entries, builtin tables, and handler frames are out of scope.
  Aliases may grow their own lease policy later; they are not value bindings.
- **Lease metadata is not binding metadata.** `Binding { value, scheme }` remains
  a language object. A lease record is a host-policy object related to the live
  scope by `name + generation`. It does not live inside `Binding`, and it is not
  carried by closure-captured `Env` snapshots.
- **The epoch is deterministic.** The first epoch is the exarch ral-tool-call
  generation:

  ```text
  type LeaseEpoch = u64
  ```

  Core stores and compares the host-supplied epoch. Core does not call a clock.
- **Use renews the lease.** Expiry is by idle ral-call age:
  `epoch - last_used`, not creation age.
- **Generation protects prune.** Rebinding a name creates a new generation. A
  prune request deletes only the `(name, generation)` cohort it observed; a
  stale request cannot delete a newer binding.

## Ledger lifetime

The ledger lives on `Shell::local`, beside other host/session scratch, not on
`Binding`, `Env`, or `Mobile`.

- **Not `Binding` or `Env`.** `Env` is copied into closures and thunk bodies.
  Lease state is top-level session policy and must not travel with lexical
  scope.
- **Not `Mobile`.** Exarch snapshots and restores `Mobile` for panic recovery.
  A ledger in `Mobile` would roll back completed lease observations or leak into
  child shells. Pruning still mutates the live `Env`, so exarch must refresh its
  durable `Mobile` snapshot after any committed prune; otherwise the snapshot
  would retain old values and could resurrect pruned names after an unrelated
  worker panic.
- **Not blindly inherited.** A forked session gets a fresh ledger. It may inherit
  the parent's baseline names, but not live lease records. Inherited
  non-baseline scratch becomes ordinary scratch in the child and receives the
  normal first-boundary grace.

The ledger shape is core-owned:

```text
struct Leases {
    baseline: HashSet<String>,
    records: HashMap<String, LeaseRecord>,
}

struct LeaseRecord {
    generation: u64,
    last_used: LeaseEpoch,
}
```

`baseline` is the genesis watermark. A fresh exarch root seals it after boot,
scratch/session variable seeding, and the agent library load. `/clear` rebuilds
the shell and seals a new baseline. A fork copies the already-known baseline
names rather than sealing the parent's whole current scratch scope.

## Behavioural seam

Core exposes lease behaviour, not `Env` representation:

```text
Shell::seal_lease_baseline()
Shell::reconcile_leases(reads, writes, epoch)
Shell::prune_idle_leases(max_idle_calls, epoch) -> Vec<PrunedBinding>
```

The ordinary success path may fold reconciliation into
`Shell::run_turn(src, TurnRequest) -> TurnReport` by placing an optional lease
epoch in `TurnRequest`. With no epoch, bare ral and tests have no TTL. With an
epoch, core reconciles the ledger at the same commit boundary that installs the
turn's `Mobile`. The standalone accessors remain the semantic operations and the
unit-test seam.

Reconciliation is one correspondence check between the ledger and the session
top-level scope:

- A live session name with no record becomes a scratch record unless it is in the
  baseline.
- A top-level rebind bumps generation and resets `last_used`.
- A read name renews `last_used`.
- A record whose name is no longer live is dropped; the program removed or
  shadowed it.

Every top-level write must route through a lease-aware shell operation. Direct
`Env::set` / `set_binding` at the top level is not a valid install path for
source, plugins, recursive definitions, destructuring rest binds, or host-seeded
variables. The generation guard is only sound if every rebind passes through the
same cold path.

## Use tracking

The first implementation is **static and turn-scoped**.

- The compiler/typechecker already resolves a turn against `SessionSchemes`.
  `run_turn` can return, or internally observe, the accepted turn's session read
  set and top-level write set at commit.
- The read set includes value-position variables and command-position binding
  heads. It must include unchecked session bindings too: scheme instantiation
  alone is insufficient because `SessionSchemes` can contain names whose scheme
  is `None`. An implementation may walk the typed IR and intersect with the
  session top-level keyset, or seed root-scope name markers for every session
  name, including unchecked ones.
- The read set may be conservative. False positives keep scratch longer; false
  negatives cause surprising deletion and are bugs.
- The write set names every top-level install. It is a lease-generation input,
  not a heuristic; missed writers are ABA bugs.
- Runtime lookup remains a pure `Env` lookup. If dynamic uses later appear that
  static tracking cannot see, add a low-cost core-local per-turn touch collector;
  do not add host callbacks to `Env::get`.

A failed parse/typecheck has no accepted read set and does not renew live
leases. The next undefined-name error remains ordinary `undefined variable`;
the committed prune event in the transcript/log explains the deletion.

This deliberately does not chase names captured inside existing closures. A
closure that captured a value can keep using it after the top-level name is
reaped; that is lexical scope doing its job.

## Reaping policy

Exarch reaps only when no ral evaluation is running and no tool-result batch is
half-committed: after a `ral` tool call settles and its result is committed,
before the next provider request, or at another `ReadyForUser` boundary. The
policy is conservative:

- **Baseline names stay.** Prelude entries are excluded by scope. Baseline names
  and live top-level handles are pinned. A live handle remains cancellable by
  name; the worker ceiling from
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
  is still the backstop for lost handles.
- **Settled handles are ordinary scratch.** Once a handle has settled, its
  binding may be reaped by the same idle policy as any other value; captured
  stdout and stderr make it a good candidate.
- **Idle expiry is call-count based.** Reap an unpinned binding when `last_used`
  trails the current ral-tool epoch by **256 ral calls**.
- **Fresh state gets one boundary.** A newly-created or just-used binding is
  protected for the boundary that observes it, so state is not deleted before the
  model has had one subsequent chance to name it.

No tombstones are part of the first implementation. A prune returns
`Vec<PrunedBinding>` and exarch records the event in the transcript/log.

No retained-size estimate is part of the first implementation. Size accounting
is hard to do honestly while closures and handles can retain values behind
`Arc`s, and call-count expiry targets the pressure source: long agent sessions
issuing many ral tool turns.

## Ctrl-`\`, `/clear`, and durable work

The reaper is not a panic button.

- **Idle reap** removes stale names from the session top-level ledger and reports
  the removed names to exarch.
- **Ctrl-`\`** cancels the durable root with `RootAbort`, reaping foreground work
  and detached workers. It does not delete lexical bindings. Bindings that hold
  aborted handles remain, and `poll` / `await` can report that the worker was
  aborted.
- **`/clear`** or a session reset drops the shell and all bindings by
  constructing a fresh root shell as [[map/exarch/session|session]] already
  describes.
- **Long-running durable jobs are separate.** A future durable-job mechanism
  may move a handle out of ordinary scratch into a host-managed registry with
  explicit listing, cancellation, and lifetime policy. Do not encode that as a
  longer per-binding TTL. Avoid the spelling `disown` unless the REPL POSIX
  meaning is deliberately reconciled.

## Consequences

- The model can use ral as scratch without growing the session forever.
- The language remains boring: no binding disappears in the REPL because a host
  policy ticked.
- Diagnostics remain simple: undefined names are undefined; prune events live in
  the exarch transcript/log.
- Physical memory reclamation is best-effort until closure capture narrows; this
  ADR removes names and future seeds, not every historical reference.
- The accessors keep exarch on the embedding seam: core decides how leases
  correspond to live bindings.

## Alternatives considered

- **Creation-age expiry.** Rejected: an old but active definition is not garbage.
  Use renews the lease, so expiry is by idle age.
- **Wall-clock expiry.** Deferred: time-based deletion is more surprising and
  harder to test. Call-count expiry matches the actual pressure source and can
  be extended later with a host-supplied clock if quiet multi-day sessions need
  it.
- **Explicit pin syntax.** Deferred: baseline pins and live-handle pins are
  enough for the first implementation. If users need durable scratch, add an
  exarch host command later, not ral syntax.
- **Retained-size eviction.** Deferred: retained size is not cheap or exact while
  closure snapshots and handles can keep values alive. Idle call-count reaping
  is useful without pretending to measure heap ownership.
- **Tombstones.** Deferred: they improve recovery diagnostics but are not part
  of correctness. The first version records prune events in the exarch
  transcript/log and leaves ordinary undefined-name diagnostics alone.
- **Lease fields on `Binding`.** Rejected: it mixes language state with host
  policy, makes closure snapshots carry lease state, and expands every binding
  for a policy only exarch observes.
- **Lease table on `Mobile`.** Rejected: `Mobile` is the panic-recovery snapshot
  and child-shell inheritance unit. Lease records belong to the session host
  boundary, not to dynamic lexical state.
- **Runtime touch on every lookup.** Rejected initially: it taxes the hot path and
  entangles the evaluator with host policy. Static per-turn read sets are enough
  for lexical names; add a core-local touch collector only if evidence demands
  it.
- **Make Ctrl-`\` delete bindings too.** Rejected: root abort is cancellation of
  running work, not session reset. `/clear` already names reset.
- **Let exarch inspect `Env` directly.** Rejected by
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]];
  the host receives accessors, not representation.

## Open questions

- Should aliases eventually be leased by a parallel handler-frame policy?

See also [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]],
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]],
[[map/core/shell-state|shell-state]], and [[map/exarch/session|session]].
