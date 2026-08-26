---
status: accepted
---

# Stream depth must not be bounded by the stack

**A lazy stream is a chain of closures — block → captured env → scope →
binding → block — so any walk that crosses the captured-env link recursively
spends stack once per line. Exactly two walks cross it: the serial encoder and
drop glue. Both now cross it flat. `intern_scope` only reserves an id and
queues the scope; `InternCtx::finish` — the table's sole accessor — drains the
queue. Teardown is cut by a thread-local drop trampoline on `Closure`: glue
still does all traversal, but a closure dying inside another closure's drop
hands its bindings to that dismantler's queue instead of letting glue recurse,
keeping the stack between any two links constant. No wire change, no new
dependency, no stream redesign.**

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

- **Interning reserves; `finish` encodes.** The old `intern_scope` encoded a
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

- Pure-data depth (a deeply nested list/variant with no env links) still
  recurses in every walk, this module's included — such values cannot be built
  without tripping other recursion limits first.
- `WireDecoder::for_shell` builds one row per pass over an n-link chain —
  O(n²) passes. Correct, and cheap at the sizes this unblocks; a Kahn-style
  topological build is available if streams ever cross the wire at 10⁵ links.
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
