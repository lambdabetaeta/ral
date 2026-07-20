---
status: active
---

# Exarch leases agent scratch bindings; ral bindings do not expire

> [[decisions/260705_leases-and-budgets|leases-and-budgets]] answered this
> page's host-pins question: durable jobs need no pins at all — the worker
> registry retains the handle itself, so pruning a top-level name never
> strands a job. This page was itself rewritten, 2026-07-05, around a
> clean-room single-writer ledger design worked out after
> `260705_leases-and-budgets` landed; the mechanism below supersedes the
> generation-cohort design this page originally proposed, moved to Alternatives.

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

By the time this mechanism landed, [[decisions/260705_leases-and-budgets|leases-and-budgets]]
had already repealed the old worker "death-clock" (a hard age ceiling on a
`spawn`) in favour of an **idle-observation lease** — reaped when *unobserved*,
not when *old* — riding a universal per-shell worker registry
(`core/src/types/shell/workers.rs`) that retains every detached handle. This
page's binding lease is the second lease on that same table, reusing the idle
clock the worker lease established: expiry is by *idle* age, never creation
age, and — because the worker registry now exists and retains handles
independently — the pin walk below needs no registry of its own to avoid
stranding a job; it exists only to keep the reaper from surprise-deleting the
name of a *running* worker.

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

Exarch installs a **per-agent binding lease ledger** on that agent's `Shell`,
unlocked: `core/src/types/shell/bindings.rs`'s `BindingLedger` sits on
`LocalState` beside the [[map/core/shell-state|worker registry]], but where the
registry needs `Arc<Mutex<…>>` because the reaper daemon and worker threads
write it, the binding ledger has **exactly one writer** — the agent's own
driving thread, which owns `&mut Shell` for every turn, every install, and
every prune. A host that never arms it (the REPL, batch, worker shells,
pipeline children) pays one branch per turn door and observes no expiry ever;
`BindingLedger::default()` is that inert state.

**The single-writer argument.** Every place exarch's `Shell` is reached is
`Agent::transport.shell_mut()`, and every `Agent` is driven by exactly one
dedicated OS thread running `Agent::drive` — the trunk's worker thread in the
TUI, the one drive call in headless, and each forked sub-agent's own spawned
thread; no two threads ever hold the same `Agent`. `/clear` and `/resources`
reach the shell only by routing through `Control::command`, itself called from
inside `drive`'s loop on that same thread — never from the UI/render thread,
which only touches the bus and the fleet's shared registry. The reaper daemon
thread that fires worker leases never touches `local.bindings`; it only takes
the worker registry's own lock. A lock on the binding ledger would therefore
document a race that cannot happen.

### Arming and the baseline seal

One host verb, `Shell::arm_binding_lease(lease: BindingLease)`, seals every
name currently visible anywhere in the scope chain — prelude, agent library,
rc bindings, host seed vars — as permanently exempt (`baseline`), then starts
the ledger's own clock. Exarch calls it in the two places an agent's shell is
installed — `Agent::assemble` (covering the trunk and every fork) and
`Agent::replace_shell` (covering `/clear`) — exactly where the first durable
`MobileSnapshot` is minted, so seeding, arming, and checkpointing stay one
visible sequence. A shadowing model `let` at top level is itself a baseline
name and therefore never pruned, so pruning can never un-shadow an older
meaning. A forked child's baseline is *everything inherited* — the parent's
scope, prelude included — so a fork's lease only ever governs names the child
itself writes, never a parent's scratch it happened to receive. **No
`TurnRequest` field carries the lease**: worker leases ride the request because
worker lifetime is a per-frame decision that must flow into nested spawns;
binding-lease policy is a per-shell decision, constant for the agent's whole
life, so arming the shell once states it where it lives and forecloses an
incoherent "some turns leased, some not" state against one ledger.

### The clock is the agent's own committed-turn count

The ledger's epoch ticks once per `Shell::run_source_turn` — a *committed*
source-door turn, whether or not it fails downstream — at entry, before
compilation. For exarch, every `ral` tool call is exactly one
`run_source_turn` dispatch, so **this epoch and `Agent::ral_epoch`** (the
worker-retention clock [[decisions/260705_leases-and-budgets|leases-and-budgets]]
built) **coincide one-to-one** — two ticks of the same drum, read by two
different ledgers for two different purposes. A turn that fails parse or
typecheck ticks but renews nothing, which is the correct non-vacuous reading of
"a failed turn renews nothing": age accrues, interest does not.

### The install chokepoint

Every persistent top-level write funnels through one fused verb,
`Shell::install_scope_binding(name, binding)`: it classifies the write by
`Env::at_session_scope()` — true only when the innermost scope is the
persisted user scope, false for any block/lambda/letrec/`use` frame — and
stamps the ledger exactly when the predicate is true. A fresh non-baseline
name starts its lease; a rebind renews it, because writing a name is itself
interest in it. The evaluator's four writers (`assign_pattern`'s `Name` and
`...rest` arms, `eval_letrec`'s group reinstall and its own pushed
fixpoint pre-install) all route here, so "write a scope entry" and "stamp the
ledger" cannot be pulled apart at a call site. Host verbs (`bind_value`,
`set_var`) stay on the raw `Env` primitive: every exarch call to them precedes
arming, so the baseline seal covers them without a special case.

### Use is observed, not polled

The renewal signal is a committed turn's **statically harvested reference
set**, read off the compiled IR at three seams sharing one exhaustive walker,
`ir::referenced_names(comp)`:

1. The turn's own compiled program, harvested and renewed right after
   `compile_turn` succeeds and before evaluation runs.
2. Runtime-compiled code — `source`, `use`, capability files, plugin loads —
   all pass through one shared compile seam, `check_source`; the harvest runs
   there too, because the load is executing inside an already-committed turn.
3. A runtime-resolved command head (`classify_command`'s `Resolution::Env`
   arm) touches the single name it resolved — a dispatch-time renewal, not a
   lookup-time one, so `Env::get` stays untouched and constraint 9 (zero cost
   on the pure-lookup path) holds exactly.

The walker matches every `CompKind` and `Val` variant exhaustively — no
wildcard arm — so a new IR shape is a compile error here rather than a
silently unharvested reference, and it over-approximates by design: a name in
an untaken branch still renews, which only ever lengthens a lease, never
shortens one. A closure needs no renewal at all to stay correct: it captured
an `Arc<Env>` snapshot at creation, and `Env`'s scopes are copy-on-write, so
pruning the live session scope never mutates what a closure already holds.
Only *new turn text* naming a pruned binding sees the ordinary undefined-name
diagnostic.

### Pruning: one verb, a checkpoint you cannot forget to take

`Shell::prune_idle_bindings() -> Option<(Vec<BindingPruneNotice>, MobileSnapshot)>`
is the whole prune operation, invocable only at session scope (a mid-frame
caller, such as a lifecycle hook, is refused rather than allowed to unset from
a transient frame). One pass, in deterministic (sorted) name order, over every
entry idle past the armed bound:

- **Absent from scope** (its install was rolled back by a panic restore): drop
  the ledger entry silently — an orphan, not a prune, so no notice.
- **Pinned** (its value structurally reaches a still-`Running` handle): skip
  without renewing — being pinned is not being used, so it is re-examined at
  every later boundary and falls at the first one after the handle settles.
- **Otherwise**: `unset` the name — which removes its scheme too, in the same
  act, since a `Binding` couples both — drop the ledger entry, and record a
  notice.

**Adoption sweep.** Afterward, any name in the session's top scope that is
neither baseline nor already tracked gets a fresh lease starting now. This
turns a missed install path — should the enumeration in this page's
implementation ever have a gap — from "immortal untracked name" into "leased
from first sighting", the conservative direction; the sweep runs (and may
itself mutate the ledger) even on a pass that prunes nothing.

**The signature is the safety net.** The verb returns `None` when nothing was
pruned, and otherwise returns the notices **paired with a `MobileSnapshot`
taken after the prune** — a caller cannot obtain one without the other. Exarch
installs that snapshot as `Agent::durable` in the same statement that reads the
notices, so any later panic-recovery rollback lands on post-prune state by
construction; a pruned name is structurally unresurrectable. This is why no
generation cohort is needed on the write side either (see Alternatives): a
prune and a rebind can never interleave, because both happen only on the one
thread that also owns the only `&mut Shell`.

### Pinning: the registry does the retaining, the walk only refuses surprise

`pins_running_work(value)` walks `List`/`Map`/`Variant` payloads looking for a
`Value::Handle` whose state reads `Running`, and — deliberately — never
descends into a `Lambda` or `Block`'s captured `Arc<Env>`: chasing that graph
is the retained-size walk this page and
[[decisions/260705_leases-and-budgets|leases-and-budgets]] both refuse. A
handle reachable only through a closure capture is not "the name of live
work"; it costs nothing either way, because the worker registry retains the
handle itself regardless of whether any top-level name still points at it.
`HandleState` is monotone (`Running → {Completed, Cancelled}`), which is what
makes the check-then-unset sequence race-free in the only direction that
matters: the walk may see `Running` an instant before a worker settles and
conservatively skip, never the reverse.

### `/clear`, fork, and panic recovery

- **`/clear`** replaces the whole `Shell`, so the old ledger — and any notion
  of "expired" it held — dies with it; the rebuilt shell is re-armed and
  re-sealed by `replace_shell`, the same call site that installs it. No
  generation counter is needed here either: nothing can settle across a clear
  into a ledger that no longer exists.
- **`fork_session`** hands the child a fresh, inert ledger and a snapshot of
  the parent's whole scope; `assemble` then arms the child, sealing everything
  inherited as its baseline, so a fork's lease governs only what the fork
  itself writes.
- **Panic recovery.** Two directions, both closed: a prune followed by a panic
  cannot resurrect anything, by the checkpoint-pairing rule above; an install
  followed by a panic rolls the scope back but leaves the ledger's entry
  behind on `LocalState` (deliberately not part of the rolled-back `Mobile`) —
  an orphan the next prune pass drops silently, with no notice and no event.

## Implementation parcels

The mechanism above lands as six independently-testable commits (the seventh
below is this page's own rewrite):

0. **This rewrite** — no code, wiki only.
1. **`BindingLedger` on `LocalState`.** The data shapes (`BindingLease`,
   `BindingPruneNotice`, `BindingLedger`), `arm`/`armed`/`tick`/`note_install`/
   `renew`/`renew_one`, and `Shell::arm_binding_lease` — plus the
   single-writer verification this whole unlocked design leans on, documented
   in the module.
2. **The install chokepoint** — `Shell::install_scope_binding` and the four
   evaluator writers routed through it.
3. **Use observation** — `ir::referenced_names`'s exhaustive walk, the tick in
   `run_source_turn`, and renewal after `compile_turn`, in `check_source`, and
   at `Resolution::Env`.
4. **The prune verb** — `pins_running_work` plus `Shell::prune_idle_bindings`:
   guards, the sweep, adoption, notices paired with the checkpoint.
5. **exarch wiring** — the 256-idle-call constant, arming at the two shell
   install sites, the drive-loop boundary drain, and the transcript/TUI event.
6. **Large-binding soft-threshold warning**
   ([[decisions/260705_leases-and-budgets|leases-and-budgets]] §"Shell
   residency is lexical state plus host leases"): a shallow, non-descending
   size estimate on `Value`; the lease gains a byte threshold; the install
   chokepoint queues a transcript/TUI-only notice when an armed, session-scope
   install meets it; `/resources` gains a leased-count and largest-binding
   row. A residency nudge, never an eviction trigger — the binding itself is
   untouched, and the card recommends a file path over captured bytes.

## Consequences

- The model can use ral as scratch without growing an agent shell forever.
- The language remains simple: ral bindings do not expire; exarch scratch
  names may be reclaimed by a host policy, and the reclaiming host needed no
  new capability, no lock, and no version counter to do it safely — the
  single-writer thread already made the hazards this page originally guarded
  against (a stale prune racing a rebind) unrepresentable.
- Core owns the correctness boundary: the ledger, the static access harvest,
  and handle pinning all live beside the representation they interpret, and
  the returned-checkpoint signature makes forgetting to refresh
  `Agent::durable` a type error rather than a reviewer's job.
- Physical memory reclamation is best-effort until capture narrows. This
  removes future names and future type seeds, not every historical reference.
- Shell residency now has two host policies instead of one: an idle lease on
  *lifetime* (this page) and, from parcel 6, a soft *size* warning — the two
  axes [[decisions/260705_leases-and-budgets|leases-and-budgets]] names
  "lifetime is a lease, residency is a budget" — kept genuinely separate: a
  large binding is never evicted early for its size, and an old one is never
  kept alive for being small.

## Alternatives considered

- **Creation-age expiry.** Rejected: an old but active definition is not
  garbage. Use renews the lease, so expiry is by idle age.
- **Wall-clock expiry.** Deferred: time-based deletion is more surprising and
  harder to test. The committed-turn epoch is the agent's own notion of
  activity, and it is already shared with the worker lease's retention clock.
- **Explicit ral pin syntax.** Deferred: baseline names and the running-handle
  pin are enough. If users need durable scratch, that is an exarch host
  command, not language syntax.
- **Size-based eviction.** Rejected as a lifetime trigger (parcel 6 adds a
  *warning*, never an eviction): retained size under `Arc` sharing is
  unknowable without the graph chase this page refuses throughout, and
  residency is a budget problem, not a lease trigger.
- **Lease fields on `Binding`.** Rejected: it mixes language state with host
  policy, makes closure snapshots carry lease state, and expands every
  binding for a policy only exarch observes.
- **Lease table on `Mobile`.** Rejected: `Mobile` is the panic-recovery
  snapshot and fork/session snapshot unit. A ledger there would have its clock
  rewound and its pruned leases revived by a panic rollback, and would ride
  along into every forked child. Lease bookkeeping is host-local state, on
  `LocalState` beside the worker registry.
- **Runtime touch on every lookup.** Rejected: it taxes the hot path and
  entangles pure lexical lookup with host policy; worse, closure-body lookups
  hit captured snapshots and would renew a name whose *live* binding the model
  has already abandoned. Static per-turn sets are enough for the current
  language.
- **Make Ctrl-`\` delete bindings too.** Rejected: root abort is cancellation
  of running work, not session reset. `/clear` already names reset.
- **Let exarch inspect `Env` directly.** Rejected by
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]:
  the host receives operations, not representation.
- **Generation-cohort `(name, generation)` guards** (this page's own original
  design). Superseded: the ABA race it guarded against — a stale prune
  deleting a newer rebind of the same name — is unrepresentable once the
  ledger has exactly one writer thread that also owns the only `&mut Shell`
  a prune or an install could run against. A guard against an impossible
  interleaving is dead code asserting a possibility the structure already
  excludes.
- **Threading the lease epoch through `TurnRequest`** (the `detached_lease`
  precedent, reused for the wrong axis). Rejected: worker leases are
  frame-scoped and must flow into nested spawns; binding-lease policy is
  shell-scoped and constant for the agent's life. Per-request threading would
  invite "this turn unleased" incoherence against one persistent ledger — the
  worker-lease precedent is right for workers and wrong here.
- **Drain-buffer notices for prunes**, mirroring the worker registry's
  `ReapNotice` ledger. Rejected: the registry's notices are produced on the
  reaper daemon thread and consumed later by the host, so a drainable buffer
  is the honest shape for an asynchronous producer. A binding prune is
  produced by the very call that consumes it — reusing the buffered shape
  here would imply a producer that does not exist, so the prune verb returns
  its notices directly instead.
- **Prune inside core, folded into `run_source_turn`'s own entry.** Rejected:
  it superficially removes the host's pairing obligation, but under panic
  rollback a resurrected name whose ledger entry was already deleted becomes
  untracked, forcing tombstones or entry retention back in — and it moves a
  host-policy moment inside the interpreter. The returned-checkpoint
  signature achieves the same safety with strictly less state.
- **Tombstones.** Rejected: they would exist only to detect a resurrection the
  checkpoint-pairing rule already makes impossible. The transcript event is
  the paper trail a later "where did `x` go?" needs; no in-language marker is.

## Open questions

- Should aliases eventually get a parallel lease policy over removable
  handler frames?

Resolved, 260705:

- **Prune events are transcript/display facts only, never model-visible tool
  context.** Constraint 7 holds throughout: no `events.json` twin, no inbox
  message. The only model-facing consequence of a prune is the ordinary
  undefined-variable diagnostic, unmodified, should the model name the pruned
  binding again.
- **Host pins dissolve.** No future durable-job or schedule registry needs to
  pin binding names or handle ids against this reaper:
  [[decisions/260705_leases-and-budgets|leases-and-budgets]]'s worker registry
  retains the handle itself, so pruning a top-level name can never strand a
  job, and [[decisions/260617_scheduled-wakeups|scheduled-wakeups]]'s
  `ScheduleId` needs no pin either, for the same reason.

See also [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]],
[[decisions/260617_long-running-work|long-running-work]],
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]],
[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]],
[[decisions/260705_leases-and-budgets|leases-and-budgets]],
[[decisions/260705_session-ledger|session-ledger]],
[[map/core/shell-state|shell-state]], and [[map/exarch/agent|agent]].
