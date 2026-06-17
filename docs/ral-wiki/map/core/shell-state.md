---
generated_at_commit: 1f8cb95d
generated_at_date: 2026-06-15
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
- `cwd.rs` (`Cwd`), `inherit.rs` (parent⇄child transfer), `modules.rs`,
  `control.rs`, `repl.rs` (`ReplScratch`, owned by the [[map/repl|REPL]] layer).
