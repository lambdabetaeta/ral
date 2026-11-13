---
status: active
generated_at_commit: e67000a54ec3cbecefa1b52c3c593d70d98bc7f2
generated_at_date: 2026-11-11
verified_at_commit: e67000a54ec3cbecefa1b52c3c593d70d98bc7f2
verified_at_date: 2026-11-11
anchors: [Frame::Acquire, Frame::Guard, Frame::Cleanup, ExitCase, guard_release, exit_case, would_run, Shell::shield, Scope::shielded, Cradle, spawn_child, launch_thread_stage]
---

# A guard acquires

**`guard ACQUIRE BODY RELEASE` owns acquisition and release as one dynamic
extent; the release observes the body's ending but cannot consume it.**
Agreed 2026-11-10 and 2026-11-11. This amends
[[260819_a-failing-cleanup-pre-empts|a-failing-cleanup-pre-empts]] and
[[261016_a-cleanup-is-shielded-within-its-grace|a-cleanup-is-shielded-within-its-grace]].

## Why

- **The acquisition window must belong to the operator.** In
  `let t = temp-dir; guard …`, cancellation during creation or between the
  binding and frame installation can leak the directory. Step-boundary
  polling makes that window routine under exarch's wall timeouts. The koala
  `in-temp-dir`, makeself `with-src` and web-search helpers exhibited it.
  Haskell's `bracket` masks acquisition for the same reason.
- **Rollback needs the ending.** Immutable bindings cannot remember whether
  to remove a staging file on failure, exit or Ctrl-C. A release inspecting
  the ending expresses that in one `case`, like `generalBracket`. Unlike
  Python's `__exit__`, returning a value cannot suppress the ending.
- **Creation must not precede a predictable removal refusal.** `temp-*`
  needs filesystem write authority, but `rm` also needs exec admission.
  Bundled `rm` exists only with `coreutils`; other builds need `PATH`.
  Without a precheck, a grant omitting or vetoing `rm` would create a path,
  refuse its release and leak it, replacing even a successful body with an
  error about an unwritten command.
- **Typing does not decide what is syntax.** `try`, `guard` and `gather`
  have ordinary HM types. Their dynamic extent needs a machine frame or
  worker, which a function over thunks cannot supply in tail position: a
  native re-enters `machine::apply` and meets `NESTED_MACHINE_LIMIT`.

## Decision

- **D1–D2: execution order, one arity.** The body takes the resource; the
  release takes the resource and ending. A resource-less guard uses
  `guard { () } { |_| … } { |_ _| … }`; no second release shape is added.
- **D3: observable endings.** `ExitCase` is the closed variant
  `` `ok | `err ErrorRecord | `exited Int | `stopped CancelCause ``.
  `ok` is nullary (its arm receives `()`); the body's value is not passed.
  `err` carries the witnessed record `try` supplies, so `raise` preserves
  its exact reason. A panic executes no release.
- **D5: bounded acquisition shielding.** Acquisition and release both use
  `Shell::shield(cleanup_grace)`. A pending stop is polled before acquisition;
  a stop during it lets it finish within the grace. The guard frame is
  installed before the body's first poll can observe that stop.
- **D6: removal remains `rm`.** `with-temp-dir` and `with-temp-file` are
  prelude functions over `guard`, releasing with `rm -rf` and `rm -f`.
  There is no native removal operation. Keep workers using the path inside
  a `gather` in the body: removal follows the body's extent, not loose work.
- **D7: `temp-*` requires `rm` before creation.** `would_run("rm", …)` uses
  `Command::from_name` → `lookup` → `admit`, including the admitted command's
  retained lookup error, without launching anything. Bundled or `PATH`
  lookup and grants are exactly the external command's; a selected refusal
  is recorded through the same admission door. Preserve its error (including
  not-found status 127 or grant denial), adding one hint naming why the
  temporary-path builtin needs `rm`. Parent write authority already covers
  the invented leaf; no separate filesystem precheck is added.
- **D8: a frame's observation is not an `Ending` projection.** `ExitCase<F>`
  is its own type because no frame observes panic. It shares the tag table
  and four payload types. The type identity is pinned against `Done::ty()`
  with only `panic` removed.

The result is the body's `F^g α`; acquisition and release grades do not
change it. In particular,
`let x = guard { () } { |_| echo hi } { |_ _| () }` captures `hi`.

## D5: bounds and evidence

**There is one budget from the strike, not one per phase.**

- `Scope::shielded` and `resolve` use the earliest `cancelled_at` above the
  shield, capped by the nearest enclosing shield's grace; `Aborted` is never
  masked. Acquisition and release therefore share `since + grace`.
- An acquisition exceeding the grace is stopped and runs no release;
  anything it already held can leak. One finishing near the deadline leaves
  little release time: once the grace expires, the release's first command
  polls and stops, possibly leaking the resource. Masking, not all host work,
  is bounded by the grace; polling is cooperative.
- Terminal SIGINT still reaches external children. A child dying under the
  mask is a process failure (`` `signaled 2 ``), not a masked stop. If this
  halts acquisition, no release runs.
- **Owned, not inherited.** The shield is in the extent's ancestry: pipeline
  stages, external children, nested machines and workers of a `gather`
  **opened inside** acquisition or release are masked with it. A worker born
  into an outside owner — an enclosing `gather`, a worker's body or the
  session — hangs under that owner's scope, not the shield. This rule is
  identical for acquisition and release; worker lifetime and cancellation
  machinery are unchanged. Delegate acquisition work through an inside owner:
  `guard { gather { let h = spawn { … }; await $h } } { |r| … } { |r _| … }`.
  Awaiting an outside owner's worker can instead halt acquisition on that
  worker's stop, despite the awaiter's shield, and run no release.
- The machine checks room for three computational frames **before** acquiring:
  the deepest release stack is `Cleanup` plus two `Arg`s. Popping `Acquire`
  or `Guard` restores entry depth; later guard reservations are assertions of
  that reserved room, never fallible exits while holding the resource.
- `Acquire` restores the scope, installs `Guard { release, resource }`, then
  calls the body without a poll between. `force` neither pushes nor polls;
  the β-step polls only after popping `Arg`. `guard_release` saves the
  untouched outcome in `Cleanup` before calling release with `exit_case`.
  Return restores that outcome; a release halt replaces it.

## Rejected: birth-place masking

**A worker follows its owner, not the scope at its spawn site.** Stages hang
under the current `scope` (`launch_thread_stage`); workers hang under
`cradle.under` (`spawn_child`). Opening `gather` captures the current scope
as its cradle. Trio likewise gives a task its nursery's cancel scopes.

Shielding `cradle.under` alongside `scope` was rejected. A node's ancestry is
fixed at birth: a worker outliving acquisition or release would keep that
mask for life. The time bound would survive — no shield masks past
`since + grace`, and a brood stops children on their own nodes — but:

- Ordinary work started in acquisition would ignore Ctrl-C for the grace.
- `spawn` would cease to be compositional: stop response would depend on
  birthplace, not only on the owner.
- Haskell's `forkIO` inherits its mask and needed `forkIOWithUnmask` to undo
  that inheritance; ral adds neither mechanism.

## D7: extent and evidence

**The precheck proves present lookup and admission, not future deletion.**

- `Command::from_name` chooses bundled `rm` when available, otherwise the
  effective `PATH`; `lookup` judges candidates and `admit` reports the chosen
  denial. `require_rm` runs before `writable_temp_dir` or exclusive creation
  in either `builtin_temp_dir` or `builtin_temp_file`.
- In `with-temp-*`, acquisition and release have the same dynamic grants and
  `PATH`: a context the body scopes ends before release. An enclosing
  `within` handler for `rm` still receives the release command; its meaning
  belongs to the handler. The precheck neither runs nor replaces it.
- A bare `temp-*` caller owns its lifetime; changing the context later is its
  responsibility. Filesystem races, removal failures and grace expiry remain
  possible; the rule prevents creation under an already known launch refusal.

## Where

- `core/src/shell/evaluator/machine.rs`: `Frame::{Acquire, Guard, Cleanup}`,
  `guard_release`, `exit_case`, entry reservation and β polling.
- `core/src/fact/ending.rs`: `ExitCase`, shared `tag` table and type test.
- `core/src/typecheck/generate/control.rs`: `Generator::guard` and
  `ExitCase::<ErrorRecord>::ty()`.
- `core/src/shell/exec/command.rs`: `would_run`, lookup and admission.
- `core/src/shell/builtins/fs.rs`: `require_rm` before either creation path.
- `core/src/process/cancel.rs`: `Scope::shielded`, `resolve`, `cancelled_at`.
- `core/src/shell/exec/pipeline/thread.rs`: `launch_thread_stage` parents
  stages under the current scope.
- `core/src/shell/builtins/concurrency/birth.rs`: `spawn_child` parents workers
  under `Cradle::under`; `machine.rs` captures scope when opening `Gather`.
- `core/src/shell/prelude.ral`: `with-temp-dir`, `with-temp-file`.

See [[design/control-operators|control operators]],
[[design/failure|failure]], [[design/endings|endings]],
[[internals/evaluator-machine|the machine]] and
[[internals/cancellation|cancellation]]. User contract: `docs/SPEC.md` §8.7.
