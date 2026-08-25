---
verified_at_commit: 19d53bb
verified_at_date: 2026-07-28
anchors: [eval_top_level, evaluate, with_thunk_body, Settled, trampoline, Mobile, Mooring, IoLoan, SessionState]
---

# The evaluator today: a trampolined tree-walker over call-by-push-value IR

The evaluator runs the typed [[internals/compilation-ladder|IR]] by recursive
descent, not as an explicit-state machine: `eval_comp`
(`core/src/evaluator/comp.rs`) walks a `Comp` and recurses into its
sub-`Comp`s on the Rust call stack, threading two things through every call —
the run's immutable `&Mooring` and one mutable `Shell`. There is one ambient
lexical environment, not per-closure environments passed as data: bindings
live in `shell.mobile.scope` (`Env`), and entering a scope
(`with_scope`, `comp.rs`) pushes a frame there and pops it on the way out
rather than building a new environment value. The one place this *is* an
explicit machine is tail position: `apply` (`core/src/evaluator/trampoline.rs`)
loops on an escaping `TailCall` instead of letting a tail call recurse, which
is what keeps a tail loop in O(1) host frames. Every other reduction —
non-tail calls, `if`/`case` arms, bind continuations — is host recursion, so
depth is capped by `recursion_limit` (default 1024 frames) to fail cleanly
before the Rust stack would overflow. *The evaluator is `core/src/evaluator/`
alone* — `comp`, `expr`, `val`, `call`, `case`, `pattern`, `scope`,
`trampoline`, `capture`, redirect frames, `audit`; the command / pipeline /
transport plumbing it delegates to lives in `core/src/runtime/` and re-enters
the evaluator only through three verbs
([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).

**Planned:** a CEK-style machine — explicit closures, frames, and one `step`
— replacing this tree-walker and its ambient scope is designed in
`dev/docs/plans/260825_cek_machine.md`; nothing below this line describes
that machine as built.

**Evaluation is entered only through the framed run door; the machine's own
verbs are crate-private.** Two verbs reach outside the module:

- `eval_top_level` (`pub(crate)`) — the run-evaluation verb a tool call, a
  REPL line, or a script line *settles through*. Hosts never call it: they
  enter through the framed `Shell::run` door
  ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]). It is a
  *resume point*: the post-run `Mobile` is installed on the shell on **every**
  outcome (Ok / Error / Exit), so `let`, `cd`, and env persist to the next run
  ([[invariants/turn-ends-ready|exchange-ends-ready]]).
- `evaluate` — a bare tail-absorbed run with no mobile contract, for callers
  already inside a session (module load, prelude bootstrap, capability
  profiles, REPL plugin / config loading). Wrapping these in a run boundary
  would round-trip a mobile they never wanted snapshotted.

`apply` (`pub(crate)`) reduces a `Value` (closure or thunk) applied to
arguments, absorbing tail signals through the trampoline. It is reached from
outside the module only through `Shell::run`'s hook arm (`Program::Hook`) or
the in-frame builtin wrapper (`crate::builtins::apply`), so a host cannot
start an unframed reduction.

**The run's frame is split by mutability.** What the run fixed — its `surface`
sink, the `deferred` rail with its worker lease and cap, the `desk`, the
`nursery`, the foreground `cancel` scope, and its terminal authority — is the
`Mooring`, which lives on the run door's Rust stack frame and is only ever
borrowed. Nothing saves or restores it, because nothing moved it: the stack
does that work, and the `NurseryGuard` beside it empties the nursery on the
unwinding path too. `&Mooring` and `&mut Shell` are disjoint borrows, so a body
can surface an event while holding the shell mutably.

**The `Shell` is partitioned into four regions by lifetime — the field name
*is* the invariant** ([[decisions/260617_turn-local-state|turn-local-state]];
[[map/core/shell-state|shell-state]]):

- *Mobile* — the persistable computation state that crosses evaluation
  boundaries and thread spawns: the lexical `scope` (`Env`), the `ControlState`
  counters (`last_status`, `call_depth`, `recursion_limit`), and the dynamic
  `Context` (cwd, env overlays, grants, handlers, args, modules). The public
  embedding seam.
- *Io* — the mutable residue of the run's frame: the pipeline-stage byte
  streams, taken on loan by an `IoLoan` at install and restored on teardown
  together with the two `Copy` registers the run owns for its life, the
  root-source register `session.root_file` and the dispatch register
  `local.audit.call_site`.
- *SessionState* — what outlives every run's teardown: the durable cancel
  `root` detached workers parent under, the `sources` registry rendered against
  after a run returns, the `exit_hints` table, the host-installed `builtins`,
  and the session's `terminal_lease`.
- *LocalState* — host-local scratch with its own flow rules: the `Audit` tree
  and REPL scratch; the residue once run and session state are named.

Hot loops poll the mooring's `cancel` scope cooperatively, through
`process::check(mooring, shell)`
([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]).

**A same-thread thunk body runs *in* the caller's session, not a copy.**
Forcing a block or applying a lambda is one β-step over one threaded store: the
runtime evaluates the body on the live `Shell` through `Shell::with_thunk_body`,
sharing run, session, and local state by identity — the mooring is the
caller's own, lent onward — and swapping in only a `Mobile` rescoped to the
closure's `captured` environment plus a fresh frame
([[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]).

- The `ThunkBody` kind fixes the only two places block and lambda differ: a
  *Block* enters with the caller's `last_status` and folds only `last_status`
  back, discarding the body's `cd`; a *Lambda* enters with a fresh
  `last_status`, binds its parameter in the pushed frame, and folds back
  `{last_status, cwd}`.
- The store the body inherits — cancel root, source registry, builtin table,
  terminal lease, audit trail — is shared by *being* the same evaluation, never
  re-attached field by field. Only a genuine runtime fork (a `spawn_thread`
  worker, a cross-process pipeline helper, a REPL aside) copies `Context` into
  a freshly-defaulted `SessionState`, and so correctly holds no terminal
  authority by default.

**The trampoline gives tail calls O(1) space.** The evaluator emits a tail call
as an internal `Control::Tail`; `apply` loops on it rather than recursing, so a
tail call lands in the loop without a new host frame and does not count against
the recursion cap (which raises a clean error before the Rust stack could
overflow). The discipline is enforced *by the type system, not a runtime guard*:
`Tail` / `TailCall` / `Control` / `Raw` are `pub(crate)`, so a tail call cannot
cross a public boundary. Callers see `Settled<Value>` (`Result<T, Break>` — tail
calls already absorbed); only the evaluator's interior sees `Raw<T>`
(`Result<T, Control>`). The seam `absorb_tail` turns one into the other at every
boundary. This is the
[[decisions/260514_completion-escape-refactor|completion-escape refactor]].

**Two exit channels.** `Break` is what `try` decides about — `Error` is
catchable, `Escape` (process `Exit`, or a `Stopped` job) propagates uncatchably
through delimited scopes. The earlier try-swallows-exit and grant tail-call
bypass bugs are fixed and regression-tested
([[decisions/260514_escape-propagation-bugs|escape-propagation-bugs]]).

**Dynamic frames nest by their own algebras** ([[design/scoping|scoping]]):

- `within` / `grant` guards push scope frames;
- the capability stack meets ([[design/grant|grant]]);
- the handler stack is deep and self-masking
  ([[design/effects-handlers|effects-handlers]]).

**The plumbing re-enters through a narrow seam.** The boundary verbs —
`eval_top_level` for a top-level run, `eval_block` for a block — always
evaluate their body in process; OS confinement is decided per-child, in
`build_command` (`crate::runtime::command::process`). A byte pipeline reaches
`runtime::pipeline::run_pipeline` ([[internals/pipeline-execution|pipeline
execution]]). The runtime climbs *back* into the machine only through
`call::invoke`, `eval_block`, and `absorb_tail` — a stage body carries closures,
so the mutual recursion is irreducible and the seam makes it visible
(`core/src/runtime.rs` names every edge).

See also [[design/cbpv|cbpv]], [[design/pipelines|pipelines]]; code maps
[[map/core/evaluator|evaluator]], [[map/core/shell-state|shell-state]],
[[map/core/runtime|runtime]]. The formal account is `docs/SPEC.md` §17.8.
