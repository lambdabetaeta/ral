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
agent, but it can also accumulate forgotten names, settled handles with captured
buffers, and large values the transcript no longer mentions.

The host must not manage that by opening `Env` itself. The
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
seam still holds: exarch supplies time and policy, and core supplies behavioural
accessors. Core owns the representation, tombstone lookup, and diagnostics.

Three facts shape the design:

- **Lookups stay cheap.** `Env::get` is on the evaluator hot path. Reaping must
  not put atomics, clocks, or host callbacks on every variable lookup.
- **The leased scope is structural.** Prelude loading leaves `Env` as
  `[prelude, session]` at a ready boundary. The reaping domain is the session
  top-level scope, not a fold over every lexical frame.
- **Pruning a binding may not free its value.** `Lambda` and `Block` values
  carry captured `Arc<Env>` snapshots, and live workers can also retain values.
  Reaping removes the session name and shrinks future seeds; physical memory
  follows only when no capture still points at the value.

## Decision

Exarch installs a **binding lease ledger** over the session top-level lexical
scope.

- **ral semantics are unchanged.** The REPL has no binding TTL. A ral program
  does not observe time-based expiry except when embedded in exarch's host
  policy. Prelude bindings, rc/profile bindings, exarch's boot library, and
  explicit durable pins are never reaped.
- **Only top-level session bindings are leased.** Local scopes, closure-captured
  scopes, prelude entries, builtin tables, and handler frames are out of scope.
  Aliases may grow their own lease policy later; they are not value bindings.
- **Lease metadata is not binding metadata.** `Binding { value, scheme }` remains
  a language object. A lease record is a host-policy object related to the live
  scope by `name + generation`. It does not live inside `Binding`, and it is not
  carried by closure-captured `Env` snapshots.
- **Time is host input.** Core stores the host's opaque lease epoch and compares
  it. Core does not call a clock. Exarch supplies both axes:

  ```text
  struct LeaseEpoch {
      ral_calls: u64,
      now_ms: u64,
  }
  ```

- **Use renews the lease.** Expiry is by idle age: `now - last_used`, not
  `now - created`. Creation time is diagnostic metadata and a tie-breaker for
  otherwise equal eviction candidates.
- **Generation protects prune.** Rebinding a name creates a new generation. A
  prune request deletes only the `(name, generation)` cohort it observed; a stale
  request cannot delete a newer binding.

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
  the parent's baseline pin names and explicit pins, but not live lease records
  or tombstones. Inherited non-baseline scratch becomes ordinary scratch in the
  child and receives the normal first-boundary grace.

The ledger shape is core-owned:

```text
struct Leases {
    baseline: HashSet<String>,
    pins: HashSet<String>,
    records: HashMap<String, LeaseRecord>,
    tombstones: HashMap<String, Tombstone>,
}

struct LeaseRecord {
    generation: u64,
    created: LeaseEpoch,
    last_used: LeaseEpoch,
}
```

`baseline` is the genesis watermark. A fresh exarch root seals it after boot,
scratch/session variable seeding, and the agent library load. `/clear` rebuilds
the shell and seals a new baseline. A fork copies the already-known baseline
names rather than sealing the parent's whole current scratch scope.

## Behavioural seam

Core exposes the lease behaviour, not `Env` representation:

```text
Shell::seal_lease_genesis(epoch)
Shell::binding_inventory() -> Vec<BindingInfo>
Shell::mark_bindings_used(names, epoch)
Shell::prune_bindings(policy, epoch) -> Vec<PrunedBinding>
Shell::tombstone(name) -> Option<TombstoneInfo>
```

`BindingInfo` reports the host currency: name, generation, value kind, pin state,
live-handle state, created/last-used epochs, and an approximate retained size
once core can compute one cheaply. It does not expose `Binding`, `Value`, or
`Env`.

The ordinary success path may fold `mark_bindings_used` into `eval_turn` by
placing `Option<LeaseEpoch>` in `TurnFrame`. With `None`, bare ral and tests have
no TTL. With `Some(epoch)`, core reconciles the ledger at the same commit
boundary that installs the turn's `Mobile`. The standalone accessor remains the
semantic operation and the unit-test seam.

Reconciliation is one correspondence check between the ledger and `scope[1]`:

- A live session name with no record becomes a scratch record unless it is in the
  baseline or explicit pin set.
- A top-level rebind bumps generation, resets `created` and `last_used`, and
  clears any tombstone for that name.
- A read name renews `last_used`.
- A record whose name is no longer live is dropped without a tombstone; the
  program removed or shadowed it.

Every top-level write must route through a lease-aware shell operation. Direct
`Env::set` / `set_binding` at the top level is not a valid install path for
source, plugins, recursive definitions, destructuring rest binds, or host-seeded
variables. The generation guard is only sound if every rebind passes through the
same cold path.

## Use tracking

The first implementation is **static and turn-scoped**.

- The compiler/typechecker already resolves a turn against `SessionSchemes`.
  `eval_turn` returns the accepted turn's session read set and top-level write
  set, or the lease frame observes them at commit.
- The read set includes value-position variables and command-position binding
  heads. It must include unchecked session bindings too: scheme instantiation
  alone is insufficient because `SessionSchemes` can contain names whose scheme
  is `None`. An implementation may walk the typed IR and intersect with the
  session top-level keyset, or seed root-scope name markers for every session
  name, including unchecked ones.
- The write set names every top-level install. It is a lease-generation input,
  not a heuristic; missed writers are ABA bugs.
- Runtime lookup remains a pure `Env` lookup. If dynamic uses later appear that
  static tracking cannot see, add a low-cost core-local per-turn touch collector;
  do not add host callbacks to `Env::get`.

A failed parse/typecheck has no accepted read set and does not renew live
leases. It may still consult tombstones for mentioned names so recovery errors
say "pruned" rather than plain "undefined". That mention collector is a
diagnostic read, not a lease renewal.

This deliberately does not chase names captured inside existing closures. A
closure that captured a value can keep using it after the top-level name is
reaped; that is lexical scope doing its job.

## Reaping policy

Exarch reaps only when no ral evaluation is running and no tool-result batch is
half-committed: after a `ral` tool call settles and its result is committed,
before the next provider request, or at another `ReadyForUser` boundary. The
policy is conservative:

- **Pinned names stay.** Prelude entries are excluded by scope. Baseline names,
  explicit pins, and live top-level handles are pinned. A live handle remains
  cancellable by name; the worker ceiling from
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
  is still the backstop for lost handles.
- **Settled handles are ordinary scratch.** Once a handle has settled, its
  binding may be reaped by the same idle policy as any other value; large
  captured stdout and stderr make it a good candidate.
- **Idle expiry is the default.** Reap an unpinned binding when `last_used` is
  older than **one day** or trails the current ral-tool epoch by **256 ral
  calls**, whichever happens first.
- **Fresh state gets one boundary.** A newly-created or just-used binding is
  protected for the boundary that observes it; memory pressure does not delete
  state before the model has had one subsequent chance to name it.
- **Memory pressure may evict sooner after that grace.** Once core exposes a
  cheap retained-size estimate, exarch may evict least-recently-used unpinned
  bindings until the scratch budget is back under its cap. Prefer settled
  handles and large byte/list/map/string values before callable definitions.

A pruned binding leaves a small **tombstone**: name, generation, value kind,
reason, and last-used epoch. Undefined-name diagnostics consult tombstones first:

```text
undefined variable: $xs
exarch pruned $xs after 143 idle ral calls; bind it again or pin durable state
```

Tombstones are bounded and expire independently — initially after seven days or
1024 ral-tool epochs. They are diagnostics, not bindings. Core stores and looks
up tombstones; host formatting may render the structured reason, but exarch does
not scrape error strings.

## Ctrl-`\`, `/clear`, and durable work

The reaper is not a panic button.

- **Idle reap** removes stale names from the session top-level ledger and leaves
  tombstones.
- **Ctrl-`\`** cancels the durable root with `RootAbort`, reaping foreground work
  and detached workers. It does not delete lexical bindings. Bindings that hold
  aborted handles remain, and `poll` / `await` can report that the worker was
  aborted.
- **`/clear`** or a session reset drops the shell and all bindings by
  constructing a fresh root shell as [[map/exarch/session|session]] already
  describes.
- **Long-running durable jobs are separate.** A future promote/retain mechanism
  may move a handle out of ordinary scratch into a host-managed durable job with
  explicit listing, cancellation, and lifetime policy. Do not encode that as a
  longer per-binding TTL. Avoid the spelling `disown` unless the REPL POSIX
  meaning is deliberately reconciled.

## Consequences

- The model can use ral as scratch without growing the session forever.
- The language remains boring: no binding disappears in the REPL because a clock
  ticked.
- The host gets useful recovery errors instead of silent `undefined variable`
  failures.
- Physical memory reclamation is best-effort until closure capture narrows; this
  ADR removes names and future seeds, not every historical reference.
- The accessors keep exarch on the embedding seam: core decides how leases,
  tombstones, and size estimates correspond to live bindings.

## Alternatives considered

- **Creation-age expiry.** Rejected: an old but active definition is not garbage.
  Use renews the lease, so expiry is by idle age; creation time remains only for
  diagnostics and eviction tie-breaks. Memory pressure handles bulky active
  scratch separately.
- **Lease fields on `Binding`.** Rejected: it mixes language state with host
  policy, makes closure snapshots carry lease state, and expands every binding
  for a policy only exarch observes.
- **Lease table on `Mobile`.** Rejected: `Mobile` is the panic-recovery snapshot
  and child-shell inheritance unit. Lease records belong to the session host
  boundary, not to dynamic lexical state.
- **Runtime touch on every lookup.** Rejected initially: it taxes the hot path and
  entangles the evaluator with host clocks. Static per-turn read sets are enough
  for lexical names; add a core-local touch collector only if evidence demands
  it.
- **Make Ctrl-`\` delete bindings too.** Rejected: root abort is cancellation of
  running work, not session reset. `/clear` already names reset.
- **Let exarch inspect `Env` directly.** Rejected by
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]];
  the host receives accessors, not representation.

## Open questions

- What is the first retained-size budget, and how precise must size estimation be
  before it is useful?
- What is the surface spelling for explicit pins: `pin`, `keep`, or a host
  command outside ral?
- Should callable definitions receive a longer idle window, or is explicit
  pinning the cleaner boundary?
- Should aliases eventually be leased by a parallel handler-frame policy?

See also [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]],
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]],
[[map/core/shell-state|shell-state]], and [[map/exarch/session|session]].
