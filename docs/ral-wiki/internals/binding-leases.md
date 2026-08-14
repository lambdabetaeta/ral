---
verified_at_commit: f7cf93a
verified_at_date: 2026-07-25
anchors: [BindingLedger, arm_binding_lease, install_scope_binding, referenced_names, prune_idle_bindings, pins_running_work, emit_ready_boundary_notices, BINDING_IDLE_CALLS, renew_one]
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
baseline, permanently exempt (`Shell::arm_binding_lease`, called where each
agent's shell is installed, so `/clear`'s rebuilt shell re-seals for free).
Bindings made inside blocks, lambdas, `use` bodies, or letrec fixpoint frames
are invisible to the ledger by the same predicate that classifies installs
(`Env::at_session_scope` at the `install_scope_binding` chokepoint): they die
with their frame anyway.

## The clock, and what counts as use

The ledger ticks once per committed source-door run — in exarch, exactly once
per `ral` tool call. No wall clock anywhere: a quiet weekend ages nothing.

Use is read off the program text, never off the running lookup path. When a
run compiles, an exhaustive walk over its typed IR (`ir::referenced_names`)
collects every variable occurrence and command-head name and renews those
entries; the same harvest runs when `source`/`use` compile code mid-run, and
a dispatch-time touch (`classify_command`'s `Resolution::Env` arm,
`BindingLedger::renew_one`) catches command heads that resolve to a binding
at runtime. Writing a name is also using it — a rebind restamps at the
chokepoint. A run that fails to parse or typecheck ticks the clock but
renews nothing: failed runs age your scratch. Listing (`/resources`, any
enumeration) renews nothing either — enumeration is not observation.

## What pruning does — and deliberately does not do

At each ready boundary — the tail of every `Shell::run` —
`Shell::emit_ready_boundary_notices` calls `Shell::prune_idle_bindings`
beside the worker-reap drain (`take_worker_reap_notices`). Every entry idle
past the bound is examined: a name whose value still structurally reaches a
*running* worker handle is pinned and re-checked next boundary
(`pins_running_work` — it recurses lists, maps, and variant payloads, and
deliberately never looks inside a closure's captured environment);
everything else is unset — value and type-scheme seed
in one act — and one dim transcript card names what fell. The verb returns
the prune notices *together with* a post-prune `MobileSnapshot` the host must
adopt as its panic-recovery checkpoint, so a later panic rollback cannot
resurrect a pruned name; the pairing is the signature, not a discipline.

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
forever. A closure captures an `Arc<Env>` snapshot at creation and resolves
its body against that captured chain; the live scopes are copy-on-write, so
unsetting `big` in the session scope cannot reach what `f` already holds.

This is correct, not a near-miss: once captured, the top-level name `big`
routes nothing. The value's real owner is `f`'s capture, and `f` — the thing
actually being used — is the thing whose lease renews. The cost is memory,
not correctness: the captured bytes stay resident until `f`'s own name falls
or is rebound and the capture drops. That residency is exactly what the
large-binding warning exists to head off — bind a file path, not five
megabytes of captured text.

One asymmetry is observable and blessed: **command-position use inside a
running body renews; value-position use does not.** The dispatch-time touch
is name-keyed and fires while a closure body runs (a thunk body shares the
shell's host-local state by identity), so if `big` is a block invoked as
`big args` inside hot `f`, every call renews the live entry — while `$big`
resolves through the captured chain and touches nothing. A false renewal
only ever lengthens a lease; the harvest over-approximates in the safe
direction throughout.

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
