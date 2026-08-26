---
status: accepted
---

# The evaluator steps closures

**The evaluator is a CEK machine.** What is in focus is a computation closure
⟨M, E⟩; the continuation is a stack of frames, each carrying the environment
it resumes under; the store is the `Shell` minus its lexical scope. One
`step`, one arm per rule. The tree-walker, its ambient scope, its trampoline
and its `Tail`/`Raw`/`Control` currency are gone.

## The founding complaint

The old evaluator recursed on the Rust stack with **one ambient environment**:
`shell.mobile.scope` was pushed and popped around every scope, so a binder's
extent was whatever happened between two calls, not a property of the term.
Tail calls escaped as a `Control::Tail` that `apply` looped on, and every
boundary had to `absorb_tail` — a discipline that leaked twice (try swallowing
exit, grant bypassed by a tail call) and that no frame in the code could be
pointed at. `Value::Lambda` and `Value::Block` were told apart by construction
and treated differently on force: a forced block *discarded* its `cd`, a
lambda folded it back — an artefact of snapshotting `Mobile`, not a rule
anyone wanted. Depth was capped at 1024 host frames. The prelude was shipped
in full as row 0 of every pipeline-stage message, and nine of its bindings
were computed at boot against *that process's* stdout, so a helper stage held
a different prelude from its host and only the wire hid it.

## What was decided

- **Closures in focus, frames carry environments.** `M to x. N` puts ⟨N, E⟩ in
  the `To` frame before M runs; `E[x ↦ v]` is built from the frame when M
  returns. Extent is structural. `a; b` is `a to _. b`; a block is a
  right-nested binder chain; a `let` inside `if`/`case` arms scopes over the
  rest of its block without a scope push (S5, S11).
- **One thunk value, and `force` is not a bracket.** `Value::Thunk(Closure)`;
  `force(thunk M) = M` pushes nothing. An unbracketed store write in any body
  — `cd`, `alias`, a hook registration — persists; `within [dir:]`/`within
  [handlers:]` are the scoped forms (S1, S10). Lambda-ness is read off the
  body's shape (`Comp::arrow`), guaranteed by the checker's η-expansion of
  every arrow-typed computation into a thunked λ (S3).
- **Two terminal shapes are a type.** `Terminal::{Value, Lambda}`; a frame has
  a rule for each terminal its hole admits.
- **Recursion is an n-ary `rec` with a projection**, not a record fixpoint —
  `Ty::Map` is homogeneous and would have forced one type on a group. Nothing
  in a body is rewritten.
- **The environment is a finite map, not the store.** Three tiers — natives,
  frozen prelude, persistent bindings — with O(1) capture. `Context` (grants,
  handlers, env overrides, dir, cwd, args, modules, hooks) is store: read in
  O(1) by policy code, changed only by frames holding undo, never captured.
  `Mobile` dissolved into `shell.env`, `shell.context`, `shell.last_status`.
- **`Define` extends the session environment forever.** The top level is
  phrases; a `Define` is installed as it lands, so a later phrase — or a
  `use` inside one — sees it, and a halted run has installed exactly the
  `Define`s that ran. `source` is a form whose value is `()` and whose halt
  halts the caller; `use` runs the module under the session environment (S2,
  S7, S12).
- **Store reads are computations.** `$CWD`, `$ENV`, `$ARGS`, `$NPROC`, `$USER`
  and `~`-paths are `Observe`, hoisted like `$[…]`; the five names are
  reserved (S8). Closing a value therefore needs no shell.
- **The cap counts frames, and is checked before the effect.** `stack_limit`
  (default 100 000) replaces 1024 host frames; `reserve` runs before any
  sink swap, redirect entry or grant push, so a refused push leaks nothing
  (S4). Nested native machines have their own limit, set by a test on a
  2 MiB worker stack.
- **The prelude is invariant by construction.** Every prelude phrase is a
  `Define` of a value and the bake rejects anything else; the nine `ansi-*`
  constants became `styled <style> …`, which asks `_ansi-ok` when it writes.
  The wire carries only the bindings tier, seated under the receiver's own
  constant tiers (S14).
- **Pipes are nodes between machines.** `Frame::Pipe(PipeNode)` owns the
  process group through collect and finish; a pipeline's outcome meets the
  parent's frames by the same rules as any focus. No frame crosses the wire.
- **Parked:** handlers and grants living on the stack as frames the
  resolution walks (S9). It changes the policy boundary and the wire for no
  observable difference; its own plan, if any.

## What it cost, and what it bought

B4 (pipeline launch) fell by an order of magnitude with the prelude off the
wire; tail loops (B2) halved; non-tail calls (B1) sped up by a fifth because a
call no longer clones `Context`. Two costs are the design's own: a native that
applies a user function runs a nested machine per element, and a `bind` into
a large persistent map copies a node path where the scope stack inserted in
place. Both are recorded, with the profile, in the plan's §8–§9; the second is
the representation trade-off — O(1) capture against an allocating bind — and
stays open.

Plan: `dev/docs/plans/260825_cek_machine.md`. Narrative:
[[internals/evaluator-machine|evaluator-machine]]. Supersedes the trampoline
account in
[[decisions/260514_completion-escape-refactor|completion-escape-refactor]]
and the body-shares-the-session bracket in
[[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]
as descriptions of the mechanism; their invariants hold by construction now.
