---
generated_at_commit: 7ba500b
generated_at_date: 2026-06-17
covers_paths: [core/src/types/, core/src/types.rs]
---

# Map: core / runtime values & shell state

`core/src/types/` defines what the [[map/core/evaluator|evaluator]] manipulates
at runtime. `types.rs` is a re-export façade so the rest of the tree spells
everything `crate::types::*`.

## Values

- `value.rs` — `Value` (the runtime [[design/cbpv|value]] category), plus the
  handler machinery: `HandlerFrame`, `HandlerStack`, `FrameHandle`,
  `BuiltinEntry` / `BuiltinTable`. Handlers are deep and self-masking, with no
  `resume` ([[design/effects-handlers|effects-handlers]], [[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]]).
- `list.rs` / `map.rs` — `List` and `Map`, opaque newtypes over persistent
  `imbl::Vector` / `imbl::OrdMap`.
- `flow.rs` — the control-flow surface: `Settled`, `Escape`, `Break`, and the
  crate-internal `Control` / `Raw` / `Tail`
  ([[decisions/260514_completion-escape-refactor|completion-escape-refactor]]). No `Option`/null appears;
  optionality is open variants ([[invariants/optionality-via-variants|optionality-via-variants]]).
- `error.rs` — `Error`, `Status`, and the `BodyResult` split. `audit.rs` — the
  `Audit` collector and `ExecNode` execution tree. `env.rs` — lexical `Env` and
  `EnvVars` process-env overrides; a scope entry is `Binding { value, scheme }`,
  so the checker's verdict rides next to the value
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
- `coerce.rs` — the `sig` / `sig_hint` runtime-error constructors and the
  `as_map` family of `Value` → `Map` coercions, sitting below both the builtin
  and capability layers so each reaches them through `crate::types::*`.

## Capabilities

`capability.rs` holds the capability *types* the [[map/core/capabilities|grant]]
decision layer interprets: `Capabilities`, `ExecPolicy`, `FsPolicy`,
`EditorPolicy`, `ShellPolicy`, `GrantStack`, `SandboxProjection`, and the `Meet`
/ `Join` lattice operations (tested in `capability/lattice_tests.rs`).

## Shell

`shell/` carves the interpreter state in two, joined by `Shell`:

- **`Mobile`** — the persistable bundle (scope + `ControlState` + dynamic
  `Context`) that survives evaluation boundaries and thread spawns.
- **`Local`** — IO, the `surface` sink, audit trail, REPL scratch, cancel scope,
  exit hints.

The `SurfaceSink` (`Local::surface`, `Arc<dyn Fn(Value)>`) is the value-typed
dual of the byte [[map/core/io-process|Io]] sinks: a host installs one for the
extent of an evaluation and the `surface` builtin forwards to it; `None` under a
bare REPL, where `surface` is the identity. It is cloned into thunk bodies and
spawned stages (`inherit.rs`), and replayed by the parent across the
[[map/core/capabilities|sandbox re-exec]].

Methods live by concern, one submodule each:

- `init.rs` — construction, env seeding into `env_overrides` only
  ([[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]);
- `context.rs`;
- `scope.rs` — `within` / `grant` guards realising [[design/scoping|scoping]];
- `checks.rs` — forwarders to the
  [[map/core/capabilities|`capability::check_*(&Context, …)`]] decisions,
  splitting the disjoint context/audit borrow for the audit-bearing checks;
- `cwd.rs` (`Cwd`), `inherit.rs` (the flow matrix, below), `modules.rs`,
  `control.rs`, `repl.rs` (`ReplScratch`, owned by the [[map/repl|REPL]] layer).

## The flow matrix

`inherit.rs` centralises *what state crosses a parent→child shell boundary* —
one file rather than a decision scattered across call sites, so no inheritable
datum (the host builtin table among them) can be silently severed by a call site
copying only the fields it happened to remember. There are two regimes.

A **same-thread β-step** — forcing a block or applying a lambda — does not fork:
`Shell::with_thunk_body` runs the body *in* the caller's `Shell`. Only the
`Mobile` is swapped, rescoped to the closure's captured `Env` plus a fresh frame;
the `turn`, `session`, and `local` state are shared *by identity*, so the body
observes the caller's audit trail, byte sinks, builtin table, cancel root, and
terminal lease without any of them being copied. There is no second store to
drift from the first ([[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]).
The `ThunkBody` kind fixes the only two places a block and a lambda differ: a
block enters with the caller's `$?` and folds only `last_status` back; a lambda
enters with a fresh `$?` and folds `{last_status, cwd}` back, so a `cd` inside a
function, alias, or handler persists like every other shell.

The owned-`Shell` modes *are* genuine runtime forks — a different store — and so
copy state explicitly. Each starts from a freshly-defaulted `SessionState` and so
holds **no terminal authority** — `TerminalAccess::Denied`, no lease — the safe
default for a store that is not the session's:

- `spawn_thread` — a spawned worker (`spawn`, `par`, the detached-worker helper)
  on a fresh OS thread that owns its own IO; nothing flows back. Runs under a
  child of the durable root, not the foreground scope, so a turn timeout or Esc
  does not reach it.
- `inherit_from` / `return_to` — the per-substate manifests a cross-process
  pipeline stage (`child_of`, [[decisions/260610_child-eval-unification|child-eval]])
  leans on. Their asymmetry *is* the flow matrix: the source cursor (`turn.loc`)
  and the `within`-attenuable bits do not flow back, but `context.cwd` does.
- `child_from` — a REPL aside (the prompt/hook shell, one call site in the
  [[map/repl|REPL plugin runtime]]): an independent sibling that clones the
  parent's `context`, source cursor, and builtin table without touching its IO /
  audit / REPL scratch; no flow-back.
- `fork_session` — the host session fork (the sub-agent case), the session-scoped
  specialisation of `child_from`. See [[map/exarch/session|session]].

Every genuine fork copies `session.builtins` (the dispatch table), so dispatch
reaches the child; the same-thread β-step shares it by identity.
