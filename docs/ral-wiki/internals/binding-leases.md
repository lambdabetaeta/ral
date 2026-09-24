---
verified_at_commit: 4dc94095
verified_at_date: 2026-09-24
anchors: [BindingLedger, arm_binding_lease, note_define, referenced_names, prune_idle_bindings, pins_running_work, emit_ready_boundary_notices, BINDING_IDLE_CALLS]
---

# Binding leases

**An exarch agent's top-level scratch names expire when the model stops using
them; nothing else about binding does.** The model treats top-level `let` as
scratch paper and never erases, so a long session accumulates dead strings,
forgotten closures, and settled handles still holding captured output. The
binding-lease ledger (`core/src/types/shell/bindings.rs`, armed only by
exarch) removes a name once it has gone 256 ral calls without use — and does
nothing else: ral's semantics are untouched, the REPL never expires anything,
and a pruned name afterwards reads exactly like a name that was never bound.
[[decisions/260629_agent-binding-reaping|agent-binding-reaping]] is the
decision record; this page is how it runs.

## What is leased, and what never is

Only names the model itself binds at the session's top level. Everything
visible when exarch arms the ledger — prelude, agent library, rc and host
seeds, and on a fork the entire inherited parent scope — is sealed as
baseline, permanently exempt (`Shell::arm_binding_lease`, armed by
`bootstrap::arm_session_ledgers` in the engine's own boot recipe and on each
fork before it is parked, so `/clear`'s rebooted engine re-seals for free).
Bindings made inside blocks, lambdas, `use` bodies, or letrec fixpoint frames
are invisible to the ledger, since its one chokepoint, `note_define`, is
reached from `run_phrases`'s `Define` arm alone, under `Mode::Session`: they
die with their frame anyway.

## The clock, and what counts as use

The ledger ticks once per committed source-door run — in exarch, exactly once
per `ral` tool call. No wall clock anywhere: a quiet weekend ages nothing.

Use is read off the program text, never off the running lookup path. When a
run compiles, an exhaustive walk over its typed IR (`ir::referenced_names`)
collects every variable occurrence and command-head name and renews those
entries; the same harvest runs when `use` (or a host loader) compiles code
mid-run. Nothing renews at dispatch: a bare head can only resolve to a
binding the elaborator already saw — prelude, session, and lexical names are
all in its bound set, and nothing installs into a running environment behind
its back — so the harvest is complete by construction. Writing a name is
also using it — a rebind restamps at the chokepoint. A run that fails to
parse or typecheck ticks the clock but renews nothing: failed runs age your
scratch. Listing (`/resources`, any enumeration) renews nothing either —
enumeration is not observation.

## What pruning does — and deliberately does not do

At each ready boundary — the tail of every run door —
`Shell::emit_ready_boundary_notices` calls `Shell::prune_idle_bindings`
beside the worker-reap drain (`take_worker_reap_notices`). Every entry idle
past the bound is examined: a name whose value still structurally reaches a
*running* worker handle is pinned and re-checked next boundary
(`pins_running_work` — it recurses lists, maps, and variant payloads, and
deliberately never looks inside a closure's captured environment);
everything else is unset — value and type-scheme seed
in one act — and one dim transcript card names what fell. A pruned name cannot
come back through a panic rollback: `Shell::run` checkpoints `env` / `context`
at *run entry*, after any earlier prune, so the rollback target
already excludes what fell. Pruning is the ready boundary's own door — reached
only between runs, on the session scope by construction — so a mid-frame caller
such as a builtin body is refused rather than allowed to unset from a
transient frame.

What it does not do: it never frees memory by itself (it removes the future
name, not the bytes), it never touches a closure, and it never tells the
model anything at prune time — the only model-visible consequence is an
ordinary `undefined variable` if the model names the binding again, with the
paper trail waiting in `record.jsonl`'s `Notice::Prune` commit.

## The capture scenario, worked

The case that looks alarming and isn't:

```
let big = <5 MB of text>
let f = { … $big … }        # then f is called every run
```

`f` renews every call — its name is in each run's text. `big` does not: its
reference lives inside `f`'s stored body, compiled once on the run that
defined `f`; later calls recompile nothing, so nothing harvests `big` again.
After 256 idle calls the live name `big` is pruned — and `f` keeps working,
forever. A closure captures its `Env` by value at creation — an O(1) clone of
a persistent map — and resolves its body against that captured scope; the
session scope is copy-on-write, so unsetting `big` there cannot reach what `f`
already holds.

This is correct, not a near-miss: once captured, the top-level name `big`
routes nothing. The value's real owner is `f`'s capture, and `f` — the thing
actually being used — is the thing whose lease renews. The cost is memory,
not correctness: the captured bytes stay resident until `f`'s own name falls
or is rebound and the capture drops. That residency is exactly what the
large-binding warning exists to head off — bind a file path, not five
megabytes of captured text.

There is no command-position exception: `big args` inside hot `f` compiles
to an application of the bound variable, exactly like `$big`, resolves
through the captured chain, and touches nothing. What a stored body mentions
is harvested once, on the run that compiled it. A false renewal only ever
lengthens a lease; the harvest over-approximates in the safe direction
throughout.

## Where it sits in the decay ladder

Abandonment decays in layers, each with its own lease and its own log line: a worker
unobserved for an hour is reaped; its settled registry entry expires after
256 unclaimed calls; and a name holding a settled handle — settled handles
are ordinary scratch, and the worker registry retains the handle itself, so
no name is ever load-bearing for rediscovery — prunes after 256 idle calls.
The model's later "where did my job go?", at every layer, has an answer in
the log.

See also [[map/core/shell-state|shell-state]] (the ledger's home on
`LocalState`), [[map/exarch/agent|agent]] (arming sites and the boundary
drain), [[invariants/probe-convention|probe-convention]] (the `/resources`
rows the ledger answers), and
[[internals/output-capture-and-detachment|output-capture-and-detachment]]
(the worker half of the same story).
