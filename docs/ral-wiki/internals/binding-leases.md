---
verified_at_commit: 3fcae877
verified_at_date: 2026-11-13
anchors: [BindingLedger, arm_binding_lease, note_define, referenced_names, Mentions, ValClos::close, ValClos::thunk, Handle::Withheld, BindingLedger::prune, pins_live_work, emit_ready_boundary_notices, BINDING_IDLE_CALLS]
---

# Binding leases

**An exarch agent's top-level names expire after `BINDING_IDLE_CALLS` (256)
source runs without use; nothing else about binding changes.** A model treats
top-level `let` as scratch and never erases it. The ledger
(`core/src/shell/runtime/state/bindings.rs`, `BindingLedger`) removes such a
name at a ready boundary; afterwards the name reads exactly as one never bound.
ral's semantics are untouched, and a host that never arms the ledger (the REPL)
never expires anything.

## What is leased

**Only names the model binds in ρ at session scope after arming.**

- `Shell::arm_binding_lease` seals every name then visible
  (`Env::all_bindings`: ρ and Σ's prelude) as *baseline*, permanently exempt.
  exarch arms it in `boot::engine_root` through
  `shell_eval::arm_session_ledgers`, after the agent library and host seeds
  are in place, so a rebooted engine reseals.
- A child built by `Shell::construct` arms from the parent's marshalled
  `Terms.lease` and seals every inherited name as baseline. Its lease governs
  only names the child writes.
- A name only Σ answers (a native, an unshadowed prelude function) is never a ρ
  entry, so it is never a candidate.
- `Shell::note_define` is the one stamp, called from `run_phrases`'s `Define`
  arm under `Mode::Session`, for every name a top-level `let`, pattern or
  recursive group installs, and never under `Mode::Local`. Bindings inside
  blocks, lambdas and `use` bodies are not session names.
- `BindingLedger::prune` ends with an adoption sweep: an unstamped,
  non-baseline ρ name gains a lease late rather than living forever.

## The clock and use

**The clock counts source runs; use is read off program text.**

- Each `Program::Source` dispatch ticks once before compiling, so a run that
  fails to parse or check ages leases without renewing them.
  `Program::Check` neither ticks nor renews. No wall clock exists.
- After a successful compile, `Toplevel::referenced_names` renews every name
  the program mentions. It is the `Mentions` walk that builds each node's
  `Occ`: a nested node contributes its stored occ, so the harvest is linear
  and a stored body's names count once, on the run that compiled it.
- Evaluation renews nothing. The elaborator saw every name a run can resolve,
  so the harvest is complete; it over-approximates, which only lengthens a
  lease.
- A rebind restamps through `note_define`. Enumeration (`/resources`,
  `Shell::leased_binding_count`) renews nothing.

## Pruning

**`Shell::emit_ready_boundary_notices` prunes once per settled source run,
while the run's frame is still installed.** A hook run and a panicked run are
no ready boundary.

- Prune runs only when the mooring has a surface sink; otherwise the ledger is
  left untouched for a later run.
- Each name idle past the bound is examined in sorted order:
  - absent from ρ (a rolled-back install): its entry is dropped silently;
  - its value reaches a live (`running` or `stopping`) handle by
    `pins_live_work`: kept and re-examined next boundary;
  - otherwise unset, value and scheme together, and reported as one
    `Notice::Prune` of `Pruned { name, idle_calls, kind }`.
- `pins_live_work` recurses lists, maps, tag spines and a partial builtin's
  supplied arguments, never a block's capture. A `Handle::Withheld` (every
  handle in a marshalled image) pins nothing
  ([[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]]).
- exarch records the notice as `Forensic::Prune` (`shell_eval/surface.rs`);
  the model is told nothing. Naming the name again is an ordinary static
  `undefined variable` error.
- `Shell::run_under` checkpoints `(env, context)` at run entry, after any
  earlier prune, so a panic rollback cannot restore a pruned name.

**Large bindings are a separate axis.** `note_define` queues a
`LargeBindingNotice` whenever `ValClos::shallow_size` meets
`large_binding_bytes` (1 MiB in exarch), whatever the name's age or baseline
status. The ready boundary writes it to stderr, sink or no sink, so it reaches
the model in its tool result.

## Captured values

**Pruning a name never touches a closure that captured it.**

```
let big = <5 MB of text>
let f = { … $big … }        # f is called every run
```

- `ValClos::thunk` and `ValClos::close` restrict a capture to the body's occ at
  formation; `close` also pulls a missing direct name from Σ with its scheme
  ([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
- Every run that calls `f` mentions `f`, never `big`; `big`'s mention was
  harvested once, when `f` was compiled. `big` expires and `f` keeps working:
  its capture holds `big`'s value, and the session ρ is persistent, so the
  unset cannot reach it.
- The bytes stay resident until `f` itself is pruned or rebound. The
  large-binding notice exists to steer the model toward binding a path
  instead.

## Decay

A settled worker's registry entry expires after `SETTLED_WORKER_RETENTION`
(256) calls unclaimed, on the same clock. A name holding a finished handle is
ordinary scratch; one holding a live handle is pinned. Each removal leaves a
record in `record.jsonl`.

See [[map/core/shell-state|shell-state]],
[[map/exarch/agent|agent]], [[invariants/probe-convention|probe-convention]]
and [[internals/output-capture-and-detachment|output-capture-and-detachment]].
