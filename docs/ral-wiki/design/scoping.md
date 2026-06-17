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

See also [[design/syscalls-are-effects|syscalls-are-effects]] (dynamic scope is for the authority over effects), [[design/cbpv|cbpv]], [[design/control-operators|control-operators]].

**Realised in** [[internals/evaluator-machine|evaluator-machine]] (the dynamic frame stack).

Cite: RATIONALE
§"Shadowing, not mutation", §"Scoped execution contexts"; `docs/SPEC.md` §3,
§3.1.
