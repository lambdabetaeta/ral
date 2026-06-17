---
verified_at_commit: 2df6db85
verified_at_date: 2026-06-10
anchors: [eval_top_level, apply, evaluate, Settled, Tail, Mobile]
---

# The evaluator as a trampolined CBPV machine

The evaluator runs the typed [[internals/compilation-ladder|IR]] as a
call-by-push-value abstract machine that threads one `Shell` of state. *The
machine is `core/src/evaluator/` alone* — `comp`, `expr`, `val`, `call`,
`case`, `pattern`, `scope`, `trampoline`, `capture`, redirect frames, `audit`,
about 2.6k lines; the command / pipeline / transport plumbing it delegates to
lives in `core/src/runtime/` and re-enters the machine only through three verbs
([[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]). Three
public verbs are the only entries from outside:

- `eval_top_level` — a tool call, a REPL line, a script line. It is a *resume
  point*: the post-run `Mobile` is installed on the shell on **every** outcome
  (Ok / Error / Exit), so `let`, `cd`, and env persist to the next turn.
- `apply` — call a `Value` (closure or thunk) on arguments.
- `evaluate` — a bare run for callers already inside a session (module load,
  prelude bootstrap, capability profiles).

**The Shell is split by mobility** — into a persistable half and a per-evaluation
half:

- *Mobile* survives evaluation boundaries and thread spawns: the lexical `scope`
  (`Env`), the `ControlState` counters (`last_status`, `in_tail_position`,
  `call_depth`, `recursion_limit`), and the dynamic `Context` (cwd, env overlays,
  modules, source position).
- *Local* is per-evaluation: pipeline-stage `Io`, the `Audit` tree, REPL scratch,
  exit hints, and the structured-concurrency `CancelScope` that hot loops poll
  cooperatively
  ([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]).

([[map/core/shell-state|shell-state]])

**The trampoline gives tail calls O(1) space.** The evaluator emits a tail call
as an internal `Control::Tail`; `apply` loops on it rather than recursing, so a
tail call lands in the loop without a new host frame and does not count against
the recursion cap (which raises a clean error before the Rust stack could
overflow). The discipline is enforced *by the type system, not a runtime guard*:
`Tail` / `Control` / `Raw` are `pub(crate)`, so a tail call cannot cross a public
boundary. Callers see `Settled<Value>` (`Result<T, Break>` — tail calls already
absorbed); only the evaluator's interior sees `Raw<T>` (`Result<T, Control>`).
This is the [[decisions/260514_completion-escape-refactor|completion-escape refactor]].

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
[[map/core/evaluator|evaluator]], [[map/core/runtime|runtime]]. The formal
account is `docs/SPEC.md` §4.
