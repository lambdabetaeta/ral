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
- **directory** scopes as local state — each saves the one working-directory cell, sets it and restores it on exit, so a `cd` inside moves only the innermost scope's copy ([[decisions/260925_within-dir-is-local-state|within-dir-is-local-state]]);
- **handler** frames by the deep self-masking discipline ([[design/effects-handlers|effects-handlers]]).

A tail-recursive call inside a `within` or `grant` block stays under that scope
across every tail landing.

## Two layers: lexical scope vs fork inheritance

The lexical scoping above is one mechanism; *crossing a shell boundary* is a
different one, and they should not be conflated.

- **Lexical scope within a shell** splits ρ from Σ. `Env`
  (`core/src/types/env.rs`) is ρ alone: a finite map from names to bindings,
  read with no fallback. `Signature` (`core/src/types/signature.rs`) is Σ —
  the natives and the frozen prelude, constants of the running shell, one per
  shell, `Arc`-shared into every fork. `crate::types::lookup(name, env, sig)`
  is the one resolution rule: ρ, then Σ's prelude, then Σ's natives. A
  binding that shadows a Σ name is an entry of ρ and wins; a name only Σ
  answers is never an entry, so `Env::restrict` never copies it.
  `Env::bind`/`extend` produce a fresh ρ that disturbs no environment a
  closure already captured, so extent is structural rather than a
  pushed-and-popped frame: `M to x. N` closes `N` over the environment the
  `To` frame carries, extended with `x`, and nothing else whatever `M` did
  along the way. ρ is one of two representations, chosen by size and
  invisible to its readers: a sorted array of at most `SMALL` (8) entries, a
  persistent hash map past it; cloning either is O(1) — the allocation is
  shared, not copied — so recursion clones; capture scrubs. A closure holds
  only the bindings its body mentions, `⟨M, ρ|occ(M)⟩`
  ([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
- **Fork inheritance** is [[map/core/shell-state|the flow matrix]] in
  `inherit.rs`: a genuine runtime fork (a `spawn` worker, a pipeline stage,
  a REPL aside, a sub-agent session) clones the parent's *session* `Env`
  whole into the new shell, and its Σ by `Arc` clone, alongside the rest of
  the parent→child manifest (builtin table, dynamic context, cancel root); a
  worker's body brings its own capture in its closure.

A same-thread β-step bridges the two: applying a thunk puts its closure's
`Env` in focus directly — `force(thunk M) = M` pushes nothing — while the
store (`Shell` minus its lexical scope) is shared by identity rather than
forked. A block and a lambda are told apart only by the body's shape
(`Comp::arrow`), never by how force treats them: an unbracketed store write in
either body — `cd`, `alias`, a hook registration — persists past the force: a
plain block is not a boundary for the store. `within [dir:]` / `within
[handlers:]` are the scoped forms, and `within [dir:]` undoes a `cd` made
beneath it
([[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]],
superseding
[[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]
as a description of the mechanism).

See also [[design/syscalls-are-effects|syscalls-are-effects]] (dynamic scope is for the authority over effects), [[design/cbpv|cbpv]], [[design/control-operators|control-operators]].

**Realised in** [[internals/evaluator-machine|evaluator-machine]] (the dynamic frame stack).

Cite: RATIONALE §"Lexical data, dynamic authority"; `docs/SPEC.md` §5, §9.
