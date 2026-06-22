---
verified_at_commit: 1baac6d
verified_at_date: 2026-06-22
anchors: [eval_top_level, evaluate, with_thunk_body, Settled, trampoline, Mobile, TurnState, SessionState]
---

# The evaluator as a trampolined CBPV machine

The evaluator runs the typed [[internals/compilation-ladder|IR]] as a
call-by-push-value abstract machine that threads one `Shell` of state. *The
machine is `core/src/evaluator/` alone* — `comp`, `expr`, `val`, `call`,
`case`, `pattern`, `scope`, `trampoline`, `capture`, redirect frames, `audit`;
the command / pipeline / transport plumbing it delegates to lives in
`core/src/runtime/` and re-enters the machine only through three verbs
([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]).

**Evaluation is entered only through framed turn doors; the machine's own
verbs are crate-private.** Two verbs reach outside the module:

- `eval_top_level` (`pub(crate)`) — the turn-evaluation verb a tool call, a
  REPL line, or a script line *settles through*. Hosts never call it: they
  enter through the framed `Shell::run_source_turn` door
  ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]). It is a
  *resume point*: the post-run `Mobile` is installed on the shell on **every**
  outcome (Ok / Error / Exit), so `let`, `cd`, and env persist to the next turn
  ([[invariants/turn-ends-ready|turn-ends-ready]]).
- `evaluate` — a bare tail-absorbed run with no mobile contract, for callers
  already inside a session (module load, prelude bootstrap, capability
  profiles, REPL plugin / config loading). Wrapping these in a turn boundary
  would round-trip a mobile they never wanted snapshotted.

`apply` (`pub(crate)`) reduces a `Value` (closure or thunk) applied to
arguments, absorbing tail signals through the trampoline. It is reached from
outside the module only through the value turn door (`Shell::run_value_turn`)
or the in-frame builtin wrapper, so a host cannot start an unframed reduction.

**The `Shell` is partitioned into four regions by lifetime — the field name
*is* the invariant** ([[decisions/260617_turn-local-state|turn-local-state]];
[[map/core/shell-state|shell-state]]):

- *Mobile* — the persistable computation state that crosses evaluation
  boundaries and thread spawns: the lexical `scope` (`Env`), the `ControlState`
  counters (`last_status`, `call_depth`, `recursion_limit`), and the dynamic
  `Context` (cwd, env overlays, grants, handlers, args, modules). The public
  embedding seam.
- *TurnState* — the dynamic frame a top-level turn installs and restores on
  teardown: the pipeline-stage `Io`, the `surface` sink, the foreground
  `cancel` scope, the source-position cursor, the detached-worker lifetime
  ceiling, and the turn's `TerminalAccess`.
- *SessionState* — what outlives every turn's teardown: the durable cancel
  `root` detached workers parent under, the `sources` registry rendered against
  after a turn returns, the `exit_hints` table, the host-installed `builtins`,
  and the session's `terminal_lease`.
- *LocalState* — host-local scratch with its own flow rules: the `Audit` tree
  and REPL scratch; the residue once turn and session state are named.

Hot loops poll the foreground `cancel` scope cooperatively
([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]).

**A same-thread thunk body runs *in* the caller's session, not a copy.**
Forcing a block or applying a lambda is one β-step over one threaded store: the
runtime evaluates the body on the live `Shell` through `Shell::with_thunk_body`,
sharing turn, session, and local state by identity and swapping in only a
`Mobile` rescoped to the closure's `captured` environment plus a fresh frame
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

**The plumbing re-enters through a narrow seam.** A top-level turn and a block
both reach `runtime::transport::dispatch`, which chooses in-process vs
OS-sandboxed child orthogonally to the block contract; a byte pipeline reaches
`runtime::pipeline::run_pipeline` ([[internals/pipeline-execution|pipeline
execution]]). The runtime climbs *back* into the machine only through
`call::invoke`, `eval_block`, and `absorb_tail` — a stage body carries closures,
so the mutual recursion is irreducible and the seam makes it visible
(`core/src/runtime.rs` names every edge).

See also [[design/cbpv|cbpv]], [[design/pipelines|pipelines]]; code maps
[[map/core/evaluator|evaluator]], [[map/core/shell-state|shell-state]],
[[map/core/runtime|runtime]]. The formal account is `docs/SPEC.md` §4.
