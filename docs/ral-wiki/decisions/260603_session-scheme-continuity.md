---
status: active
---

# Session scheme continuity

A turn's static check is seeded with the prelude schemes alone, so a name bound
in turn *N* enters turn *N+1*'s check as a bare name with a fresh type variable
— modes free, value type free. **The checker's inferred
[[map/core/typecheck|scheme]] for each top-level binding survives into the next
turn's check by living on the runtime binding itself, next to the value.**

This is the first of four decisions whose end state is a single mode-inference
engine: session-scheme-continuity (here) →
[[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]]
→ [[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]] →
[[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]. The
through-line is **net code removal**.

## The discontinuity today

- `typecheck(comp, prelude_schemes) -> Vec<TypeError>` (`core/src/typecheck.rs`)
  returns errors only. The schemes it infers for the turn's top-level binds are
  computed and then dropped.
- `compile_and_typecheck(source, bindings, prelude_schemes)` (`core/src/lib.rs`)
  takes `bindings` as a bare name set, used only by `elaborate`; the checker
  never sees a session binding's type.
- **Consequence** (see [[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]):
  the static mode verdict is vacuous on session-bound heads. The runtime engine
  `core/src/ty.rs::env_binding_type` compensates by re-inferring modes from the
  live binding at eval time. Cross-turn *value*-type errors are caught by no
  engine at all.

## What we decide

The scheme travels from checker to evaluator on the IR, and lives on the
binding in the scope. There is no separate session ledger.

- **The checker writes each top-level bind's scheme onto its `Bind` node.**
  This is the same move as [[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]
  — the checker elaborates into an annotated IR — introduced here with one slot
  (`CompKind::Bind` gains `scheme: Option<Box<Scheme>>`, boxed so the optional
  scheme stays one pointer wide on the node); ADR 3 adds the mode wires to the
  same pass.
  The verdict rides inside the compiled comp; `CompileOutcome` is unchanged.
- **A scheme leaves the checker only if it is closed.** Each turn's unifier
  dies with the turn and variable ids restart at zero, so an open scheme from
  turn *N* would alias turn *N+1*'s fresh variables. At annotation the scheme
  is resolved against the final unifier and its residual variables quantified;
  a binding that cannot be closed (the monomorphic components of a
  destructuring pattern) carries no scheme.
- **The evaluator installs value and scheme together.** The scope entry
  (`core/src/types/env.rs`) becomes `Binding { value, scheme: Option<Scheme> }`.
  The statement-level install rule that already governs the value governs its
  scheme: a rebind replaces both, a statement that never ran installs neither.
  The schemes can never drift from the values, because there is nothing to
  keep in sync.
- **The next turn's check is seeded from the live scope.** A name with a
  scheme is bound to it; a name without one — bound by a `source`d file, a
  plugin, or under `--no-typecheck` — is a bare name with a fresh variable,
  exactly as today. The two inputs of `compile_and_typecheck` (`bindings` name
  set, `prelude_schemes` list) collapse into this one name→scheme map.
- **Alias arms get the same treatment.** An `alias` installs a persistent
  handler frame (`core/src/types/shell/scope.rs::install_alias`); the arm's
  scheme is computed *at install* by the checker's arm inference
  (`typecheck::alias_arm_scheme`, the runtime handler calling convention seeded
  from the live `session_schemes()` and closed against its own unifier) and
  stored on the `HandlerEntry`, so the next turn's checker sees the alias as the
  current turn's does. All three install paths — the `alias` statement, rc
  `aliases:` maps, and plugin loads — go through it. Mode preservation for those
  arms is
  [[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]]'s rule.

## What this decision removes

- The `bindings: HashSet<String>` parameter and the per-turn
  `binding_names()` walk — subsumed by the scope's name→scheme map.
- `bake_prelude_schemes`' separate `TyEnv` walk: the bake reads the schemes
  off the annotated prelude's `Bind` nodes — one harvest, two callers
  (build-time bake, per-turn check).
- The runtime engine's reason to re-infer a session head's modes. The
  deletions themselves (`env_binding_type`, `constrain_comp`,
  `pipeline_mismatch`, the `classify` byte-mode residue) land in ADRs 3 and 4,
  once the IR carries modes the evaluator reads directly.

**Verify:**
- `$f = { return 3 }` then `$f | from-json`: the `∅`-into-`Bytes` edge is
  reported by the next turn's check, not at eval as `pipeline_mismatch`.
- `$f = { echo hi }` then `$f | wc -l`: the checker sees the harvested
  `F[∅,Bytes]` scheme, not a fresh mode variable.
- A cross-turn value-type error (turn-*N* binding used at a clashing value
  type in turn *N+1*) is reported statically.
- `x = 3` then `x = "hi"`: the next turn checks `$x` against `String`.
- `let x = 1` then a turn `x = "two"` that fails before the rebind: `$x`
  keeps the `Int` scheme — the failed statement installed nothing.
- A turn that typechecks but aborts at eval contributes no schemes.
- `let [a, b] = [1, 2]` then `$a + 1`: cross-turn use of a pattern-bound
  name neither errors spuriously nor poisons the unifier (no scheme, fresh
  variable).
- An `alias` from turn *N* is visible to turn *N+1*'s check.
- The baked prelude schemes and the per-turn schemes come from the same
  `Bind`-node harvest.

See also [[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]],
[[internals/type-inference|type-inference]], [[map/core/typecheck|typecheck]],
[[map/repl/loop|loop]], [[map/exarch/shell-eval|shell-eval]].
