---
status: accepted
---

# Stream depth must not be bounded by the stack

> **Amended 2026-11-13 — Built collections:**
> [[261112_runtime-values-are-value-closures|runtime values are value closures]]:
> built collections are ordinary closures, so the walks and the drop
> trampoline reach them as environments and captures, not as separate nodes.

> **Amended 2026-11-12 — Marshalling:**
> [[design/engine-protocol|the engine protocol]]
> replaces the encoder with one dependencies-first walk over environments
> and built collections. The reader builds rows in order, without Kahn's
> algorithm. Both walks and tag spines are flat outside IR. The runtime drop
> trampoline remains, extended to captures and built containers by
> [[261112_runtime-values-are-value-closures|runtime values are value closures]];
> the drop implementation and measurements below record the earlier decision.

**A lazy stream is a chain of closures — block → captured env → scope →
binding → block — so any walk that crosses the captured-env link recursively
spends stack once per line. Marshalling and drop glue cross it flat: the
writer's explicit stack closes dependencies before writing each row, the
reader builds those rows in order, and the runtime drop trampoline queues
uniquely owned captures and built containers. None recurses through the
value graph.**

## Context

`from-lines` builds one `` `more [head, tail] `` node per line, each tail a
`Block` capturing a one-binding `Env` holding the next node. Every other
`Value` walk — `Display`, `PartialEq`, `shallow_size`, `value_carries_handle`,
`scrub` — deliberately stops at the closure boundary, so their depth per node
is a small constant. The two walks that cross the boundary were recursive, and
both crashed in the field: a pipeline helper stage aborted encoding a stream of
a few hundred lines (`ls /tmp | from-lines` began failing when /tmp grew), and
a sixty-thousand-line stream aborted the process in drop glue at teardown.

## Decision

- **Dependencies close before their row is written.** The marshaller's
  explicit Open/Close stack visits each environment, built list or built map
  once, keyed by allocation address in one identity table. The image owns
  those allocations throughout the borrowed walk. A row names only earlier
  rows; root env and handler frames may name any row. Readback checks and
  constructs each row against its prefix, in one pass. Tag spines are flat;
  no built-data nesting consumes the host stack.

  **Historical encoder, 2026-08-06.** The original decision was “interning
  reserves; `finish` encodes”. The old `intern_scope` encoded a
  scope's bindings inline, and a binding holding a closure re-entered
  `from_runtime`, which re-entered `intern_scope` — one full cycle of frames
  per stream link. Now `intern_scope` is infallible: it assigns the id,
  reserves the row, and queues `(id, Arc)`. `InternCtx::finish(self)` drains
  the queue — encoding a scope may queue more scopes, never recurse into them
  — and is the only way to obtain the table, so no envelope can ship
  half-encoded. The queued `Arc`s double as liveness pins: an interned pointer
  cannot be reused by a fresh allocation mid-encode. Encoder stack depth is
  now bounded by data nesting inside one scope, not by stream length. The
  decoder needed nothing: `WireDecoder::for_shell` always resolved row
  dependencies rather than trusting id order, and rows were already flat.

- **A drop trampoline on `Closure`, not a hand-rolled walk.** `Value` is
  destructured by move throughout the evaluator, so `Drop for Value` is not
  writable (E0509). Every chain link passes through a `Binding` and through a
  `Closure`; the cut sat on `Binding` first (260806) and moved to `Closure`
  (260826) once the persistent map made binding drops loop-resident — every
  `bind` into a shared environment drops a copied node, one `Binding` per
  neighbour, and a list in scope was paying the trampoline on every
  iteration. Closures are rare where bindings are common. `Closure::drop`
  calls `Env::dismantle`, which keeps a thread-local queue of binding maps:
  when no dismantler is active it becomes one — dropping the map by plain
  glue, then draining the queue until quiet — and when one is already active
  above it on this thread's stack, it pushes the map and returns. The
  machine's two closure destructurings go through `Closure::into_parts`
  (`ManuallyDrop` + `ptr::read`), since `mem::take` would allocate an
  `Env::new()` per step. Glue therefore still
  performs all traversal — a shared spine stays one refcount decrement,
  nothing is ever cloned in order to be destroyed, `Arc`'s own atomic elects
  the single deallocator under concurrent drops — but the stack between any
  two links is a constant band of glue frames. An empty binding map skips
  the queue entirely; a closure dying during thread-local teardown falls back
  to plain glue, the only depth-honest option left there.

## Alternatives considered

- **An iterative `Drop for Env` walking an ownership worklist.** Implemented
  first, and rejected by measurement: it runs on *every* env drop and iterates
  the whole scope chain with copy-on-write pops, so a deep-recursion test
  (`scope_escapes`, one scope per frame, an env clone dropped per call) went
  from 14 s to spinning for 11+ minutes — O(depth) per drop on exactly the hot
  path `imbl` was chosen to make O(1).
- **Ownership traversal in `Binding::drop`** (dismantle the dying value by
  consuming it). Rejected: consuming a persistent container that shares chunks
  with a survivor *clones* elements — deep `String` copies included — in order
  to destroy the clones; an accumulator loop's rebinds would pay O(n) each,
  O(n²) overall. Only the trampoline lets glue keep the O(1) shared-spine
  drop while still cutting the stack.
- **Segmented stacks (`stacker::maybe_grow`).** Handles arbitrary depth in
  three lines, but treats the symptom in one walk, adds a dependency, and
  leaves drop glue — which `stacker` cannot reach — recursive.
- **Parent-side folding of pipe streams** (the parked pipe plan): removes this
  wire crossing for one shape, but any deep value crossing any wire would
  still overflow, and teardown would still abort.
- **A depth limit with a clean error.** A 386-entry /tmp is not pathological;
  refusing it is not a fix.

## Consequences

- Built-data nesting and tag-spine depth do not consume marshalling or
  runtime-drop stack. Other value walks and raw IR teardown retain the
  stack-safety caveat of the value-closure decision.
- The original decoder replaced repeated row scans with Kahn's algorithm,
  linear in the chain. The dependencies-first reference law now makes
  dependency recovery unnecessary: the reader constructs rows in order.
- Every non-scalar binding death costs one thread-local access and branch;
  the leader additionally allocates one small queue. `scope_escapes` measures
  at or below its pre-change time.
- Stack use in `seq 1 n | from-lines` is now O(1) in n on both sides of the
  wire and at teardown: 50k-link encode and 100k-link drop verified on
  256 KiB thread stacks.

## See also

[[map/core/transport|transport]] (the serial layer this reshapes),
[[design/codecs|codecs]] (`from-lines` and the lossy line-stream contract),
[[decisions/260609_pure-pipe-equation|pure-pipe-equation]] (why a stream is an
ordinary variant rather than a runtime-recognised type).
