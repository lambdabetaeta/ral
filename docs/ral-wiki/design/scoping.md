# Lexical scoping for data, dynamic scoping for authority

**ral splits scoping by what is being scoped** — lexical for data, dynamic for
authority:

- **Data is lexical.** `let` bindings capture at definition and are immutable, so
  `let x = 1; { let x = 2 }; echo $x` prints `1` and a closure observes the
  bindings in force where it was written.
- **Ambient authority is dynamic.** The working directory, environment overlays,
  capability restrictions, and effect handlers are inherited from the call site
  and scoped by `within` and `grant` over the body's whole dynamic extent.

The duality matches the right model to each:

- data captured at definition is predictable — this is what buys equational
  reasoning and safe `spawn`;
- what files you may touch and which commands exist must be scoped to the call
  site, so that a function defined in an unrestricted context can be invoked
  inside a restricted block and respect that restriction without code changes.

An environment variable follows the dynamic side of this split: it is read as
`$env[KEY]`, never as a bare lexical name, and a `within [env: …]` overlay is
seen by `$env`, `~`, child processes, and PATH resolution alike
([[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]).

Each dynamic frame nests by its own algebra:

- **capability** frames by intersection — each layer narrows reachability ([[design/grant|grant]]);
- **environment** overlays by shadowing — inner `KEY: VAL` overrides outer;
- **handler** frames by the deep self-masking discipline ([[design/effects-handlers|effects-handlers]]).

A tail-recursive call inside a `within` or `grant` block stays under that scope
across every tail landing.

## Two layers: lexical scope vs fork inheritance

The lexical scoping above is one mechanism; *crossing a shell boundary* is a
different one, and they should not be conflated.

- **Lexical scope within a shell** is the `Env` type (`core/src/types/env.rs`):
  three tiers checked in order — natives, the frozen prelude, and a persistent
  `imbl` map of everything bound since. `bind` is an insert into the session
  tier that disturbs no environment a closure already captured, so extent is
  structural rather than a pushed-and-popped frame: `M to x. N` closes `N` over
  the environment the `To` frame carries, extended with `x`, and nothing else
  whatever `M` did along the way. Cloning an `Env` is O(1) — the persistent
  map's root is shared, not copied — which is the hot path for recursion and
  for every closure capture.
- **Fork inheritance** is [[map/core/shell-state|the flow matrix]] in
  `inherit.rs`: when a genuine runtime fork (a `spawn` worker, a pipeline stage,
  a REPL aside, a sub-agent session) needs the parent's lexical environment, it
  clones the *whole* `Env` into the new shell, alongside the rest of the
  parent→child manifest (builtin table, dynamic context, cancel root).

A same-thread β-step bridges the two: applying a thunk puts its closure's
`Env` in focus directly — `force(thunk M) = M` pushes nothing — while the
store (`Shell` minus its lexical scope) is shared by identity rather than
forked. A block and a lambda are told apart only by the body's shape
(`Comp::arrow`), never by how force treats them: an unbracketed store write in
either body — `cd`, `alias`, a hook registration — persists past the force: a
plain block is not a boundary for the store. `within [dir:]` / `within
[handlers:]` are the scoped forms
([[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]],
superseding
[[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]
as a description of the mechanism).

See also [[design/syscalls-are-effects|syscalls-are-effects]] (dynamic scope is for the authority over effects), [[design/cbpv|cbpv]], [[design/control-operators|control-operators]].

**Realised in** [[internals/evaluator-machine|evaluator-machine]] (the dynamic frame stack).

Cite: RATIONALE §"Lexical data, dynamic authority"; `docs/SPEC.md` §5, §9.
