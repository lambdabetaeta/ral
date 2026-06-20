---
status: proposed
---

# A same-thread thunk body runs in the caller's session, not a reconstructed one

**Eliminating a thunk on the same thread — forcing a block or applying a lambda
— is one step over one threaded store; the runtime must run the body *in* the
caller's `Shell`, sharing the session by identity, never build a fresh `Shell`
whose `SessionState` is default-constructed and then re-attached field by
field.** Today blocks do the former (`with_block` swaps only the mobile on the
live shell) and lambdas do the latter (`with_child` → `child_of` →
`Shell::new(Default::default())` + `inherit_from`). The copy-and-merge path is a
manual allow-list of which session components reach the child; whatever is
omitted is silently severed. The terminal lease was omitted, so every foreground
external inside a function, alias, or handler body lost the controlling terminal
([[decisions/260619_terminal-lease|terminal-lease]], fixed as a way-station). The
defect is not the missing line; it is that a same-thread β-step allocates a
second session at all.

## The diagnosis

### The semantics has no second store

ral is call-by-push-value ([[design/cbpv|cbpv]]). A `{ … }` block is a thunk of a
value computation, `U(F α)`; a `λx. …` is a thunk of a function, `U(A → B)`; both
are `Val::Thunk` ([[decisions/260616_force-eliminates-blocks|force-eliminates-blocks]]).
Their *eliminations* differ — `!` forces a block, application pops an argument and
runs a lambda — but in the operational semantics each is a step over a single
configuration ⟨M, ρ, σ⟩: it extends the environment ρ (the lambda binds its
parameter; the block does not) and threads the store σ onward unchanged. Neither
elimination mints a new σ.

The store here is the world a `Shell` carries that outlives no single turn: the
durable cancel root, the source registry, exit hints, the host builtin table,
and the terminal lease — `SessionState`
([[decisions/260617_turn-local-state|turn-local-state]] names the four
lifetimes). There is exactly one such store per session, and a same-thread body
inherits it by *being* the same evaluation, not by receiving a copy.

### The runtime forks the store anyway

Application takes the heavy path. The `Value::Lambda` arm of the trampoline calls
`apply_lambda_frame` (`evaluator/trampoline.rs`), which wraps the body in
`shell.with_child(captured, …)` → `Shell::child_of` → `from_captured`
(`types/shell/inherit.rs`), and `from_captured` builds a *whole new `Shell`* via
`Self::new(Default::default())`. `Shell::new` constructs a full `SessionState`
and **re-mints the lease** from the `TerminalState` it is handed
(`types/shell/init.rs`); a default `TerminalState` has `startup_foreground =
false`, so the mint returns `None`. `inherit_from` then overwrites the child's
`session.builtins` and `session.root` from the parent — an allow-list — and
`return_to` folds a chosen subset back. The genuine β-binding is the
`with_scope` + `assign_pattern` *inside* that wrapper, so the new shell is strictly
an envelope around the reduction, not the reduction itself.

Forcing takes the light path. The `Value::Block` arm calls `eval_block`
(`evaluator.rs`) → `with_block` (`inherit.rs`): it clones `self.mobile`, swaps the
scope to the closure's `captured` plus a fresh frame, and runs the body on
`self`. `self.session` is never touched; the store is shared by identity. **This
is why blocks never lost the lease and lambdas did** — the difference is entirely
in the carrier, not in the semantics.

### Why a point-fix recurs

`inherit_from` enumerates the session components a child receives (`builtins`,
`root`, and now `terminal_lease`; not `sources` or `exit_hints`). It is a
hand-maintained allow-list with no totality check: any `SessionState` field added
later, or any existing one a refactor forgets, is silently dropped to its
`Default`. The lease is the worst case of that default, because `Shell::new`
does not leave it obviously uninitialised — it actively *re-derives* it from a
blank predicate and produces a confidently-wrong `None`. That is exactly the
inference-not-possession failure
[[decisions/260619_terminal-lease|terminal-lease]] set out to abolish, creeping
back in through the constructor: the child re-runs the mint instead of being
handed the one witness the session already holds. A fourth forgotten field is a
matter of time.

## The decision

**A same-thread thunk body is evaluated on the caller's `Shell`.** The
elimination swaps only the environment bundle — the closure's captured `Mobile`
scope, a fresh frame, and, for a lambda, the bound parameter — and runs the body
against the live `turn`, `session`, and `local` state. The store is shared by
identity; there is nothing to re-attach. The mobile rule stays explicit: a lambda
enters with a fresh `last_status` and folds back `{last_status, cwd}`; a block
folds back `last_status` only. Lambda-body elimination thereby converges on the
in-place mechanism the block path (`with_block` / `run_with_mobile`) already
uses, but keeps the existing lambda entry status rather than accidentally
inheriting the caller's `$?`.

The owned-`Shell` construction is **retained** only for cases that are genuinely a
*different* runtime and so must copy state:

- a spawned worker on another OS thread (`spawn_thread`: `spawn`, `par`, and the
  detached-worker helper path);
- the cross-process pipeline helper
  ([[decisions/260610_child-eval-unification|child-eval-unification]],
  `core/src/child_eval.rs`);
- the REPL aside (`child_from`: a deliberately isolated prompt/hook sibling).

`from_captured` remains the constructor primitive for those modes. `child_of`
either narrows to the cross-process helper shape or disappears if no non-test
caller needs the paired `inherit_from` wrapper after `with_child` is removed.

None of these shares a store with its parent, so each correctly receives **no
terminal authority by default** — `TerminalAccess::Denied`, no lease — which is
the safe element, not a bug to patch.

## The carrier: evaluate in place, not a shared-mutable session

The alternative carrier is to keep building a child `Shell` but make
`SessionState` *shared by reference* (an `Arc`/handle the child points at). It is
rejected:

- it keeps two evaluation paths and the copy-merge of `turn` / `local` state, so
  it fixes only the session half — a forgotten `turn` or `local` re-attachment can
  still sever state;
- it introduces shared-mutable session aliasing between parent and child, which
  the `&mut Shell` evaluator spine
  ([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]) is
  designed to avoid; and
- it pays the per-call `Shell` allocation and the lease re-mint regardless.

Evaluating in place removes the second state entirely. The mechanism already
exists and is proven (`with_block`/`run_with_mobile` swap a `Mobile` in and out
around a body on the live shell); lambda elimination becomes that routine with the
parameter bound in the pushed frame and `cwd` added to the fold-back set.

## Why this shape

- **One store, one mechanism.** Block and lambda elimination meet at a single
  in-place routine; the only differences are the fold-back set and whether a
  parameter is bound. No second `SessionState` exists to drift from the first.
- **The bad state is unconstructable, not guarded.** With no allow-list to keep
  total, no session-global datum can be forgotten — the
  [[decisions/260614_structural-bug-prevention|structural-bug-prevention]]
  discipline applied to state flow rather than to a type.
- **It matches the semantics.** ⟨M, ρ, σ⟩ threads one σ; the runtime now does
  too. The CBPV elimination forms keep their distinct typing
  ([[decisions/260616_force-eliminates-blocks|force-eliminates-blocks]]); only
  their *shared* store-threading is made literal.
- **Copy stays where copy is real.** Threads and processes genuinely fork the
  store and keep `from_captured`; their default-`Denied` / no-lease posture is
  correct.

## Alternatives considered

- **Keep copy-merge, make re-attachment total.** Force an exhaustive copy of every
  `SessionState` field — e.g. destructure the parent session at the call so a new
  field is a compile error. This is the way-station the terminal-lease fix took,
  generalised. It closes the immediate hazard but preserves a second session the
  semantics does not call for, and still pays the per-call allocation and re-mint.
  A backstop, not the destination.
- **Share the session by reference.** Above — rejected for keeping two paths and
  introducing shared-mutable aliasing.
- **Do nothing.** The lease line is fixed; the next session-global field repeats
  the bug. Rejected by the recurrence argument.

## What changes, what stays

- **New:** one in-place thunk-body elimination on `Shell` (generalising
  `with_block`), parameterised by the body kind: block or lambda. The kind fixes
  the entry mobile (`last_status` fresh for lambda), the optional bound pattern,
  and the fold-back set.
- **Changed:** `apply_lambda_frame` stops calling `with_child`; it builds a
  lambda-body mobile, binds the parameter in the pushed frame, and folds back
  `{last_status, cwd}`. `with_child` loses its only production caller and is
  removed; the terminal-lease move added to `inherit_from` / `return_to` as the
  way-station is removed with it.
- **Retained:** owned-`Shell` construction through `from_captured` for true
  runtime forks — `spawn_thread`, `child_eval`, the REPL aside `child_from`, and
  the per-call overhead bench if it remains useful; the CBPV value-level `Block`
  / `Lambda` split and their distinct eliminations; `Shell::new` minting the
  lease at *real* session construction.
- **Unchanged:** the `TerminalLease` itself and `terminal_lease()`'s access gate;
  the `Denied` default for spawned workers; the read-once stdin discipline (now a
  shared-and-bracketed read rather than a moved one).

## Consequences

- A foreground external inside a function, alias, or handler body foregrounds
  because the body *is* the session, not because a manifest remembered to copy a
  witness. [[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]
  makes this the common case: every alias and handler is a lambda.
- Per-call cost drops — no `SessionState` allocation and no lease re-mint on each
  lambda application.
- The flow matrix collapses from two asymmetric manifests to one fold-back set, so
  its asymmetry (cwd persists from a lambda, the source cursor does not) is read in
  one place.
- **Risk:** behaviours that implicitly relied on a lambda body's *isolated*
  `turn` / `local` state — its own audit subtree, its moved-in read-once stdin —
  must be re-expressed as the shared-then-bracketed semantics blocks already use.
  The test plan pins the ones that matter.

## Implementation plan

Documentation of intended work, not a commitment to build now. Each parcel
compiles and tests alone, and each removes one source of ambiguity before the
next depends on it.

1. **Name the in-place contract.** Add a `Shell` helper for same-thread thunk
   bodies that constructs the body mobile from the captured scope plus a fresh
   frame, runs on the live shell, and returns the post-body mobile. Make the body
   kind explicit: block folds back `last_status`; lambda starts with fresh
   `last_status`, binds its pattern, and folds back `{last_status, cwd}`.
2. **Route lambda application through it.** Replace `apply_lambda_frame`'s
   `with_child` call with the new helper. Preserve curried-lambda capture
   (`child.snapshot()` becomes a snapshot of the live shell while the body mobile
   is installed), tail-call absorption, the recursion counter, and the existing
   `$?` entry/exit behaviour.
3. **Retire the same-thread copy path.** Remove `with_child`. Remove the
   terminal-lease loan from `inherit_from` / `return_to`; no same-thread lambda
   should need it. Re-check the `child_of` call graph: non-test use must be only
   the cross-process helper, or the function is narrowed/renamed to that role.
4. **Audit the real forks.** Confirm `spawn_thread`, `child_eval`, and
   `child_from` still construct independent shells with `TerminalAccess::Denied`
   and no terminal lease unless a host explicitly creates a real session with
   one. Keep pipeline-stage wording tied to `child_eval`, not to `spawn_thread`.
5. **Pin the flow matrix.** Add tests for the values the carrier used to define:
   lambda `$?` entry reset and exit fold-back, lambda `cd` persistence, block
   `cd` discard, read-once stdin bracketing, audit nesting, source cursor
   non-flow-back, and no extra `TerminalLease::mint_at_startup`.

## Test plan

- **The lease, structurally.** A function / alias / handler whose body runs a
  terminal-bound external foregrounds — the point being that no manifest is
  consulted: the body observes `terminal_lease().is_some()` because it shares the
  session. Subsumes the regression test added with the way-station fix.
- **Block/lambda flow parity.** `!{ … }` still discards `let` / `cd`; a lambda
  body's `cd` still persists. A lambda enters with fresh `$?` and folds its final
  status back; a block folds only its final status back.
- **Read-once stdin.** A lambda body and a sibling call in one pipeline see the
  pipe consumed exactly once, as before.
- **Audit and source cursor.** Both body forms share the active audit trail; a
  lambda body does not flow the source cursor back to its caller.
- **No second mint.** A lambda application performs no
  `TerminalLease::mint_at_startup`; the session's witness is the same across the
  call (by identity where observable).
- **Threads and processes unchanged.** A spawned worker and a cross-process helper
  still get an independent store and `Denied` terminal access.

## See also

[[decisions/260619_terminal-lease|terminal-lease]] (the fixed symptom and the
way-station whose `inherit_from` move this retires),
[[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]
(why the blast radius is every alias and handler),
[[decisions/260616_force-eliminates-blocks|force-eliminates-blocks]] (the
`Block` / `Lambda` elimination split this leaves intact),
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]]
(unconstructable over guarded),
[[decisions/260617_turn-local-state|turn-local-state]] (the lifetime partition of
`Shell` this rests on),
[[decisions/260610_child-eval-unification|child-eval-unification]] and
[[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]] (the
owned-`Shell` cases that must keep copying), [[design/cbpv|cbpv]],
[[map/core/runtime|runtime]], [[map/core/io-process|io-process]].
