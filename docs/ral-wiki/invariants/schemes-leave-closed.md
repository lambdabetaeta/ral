# Schemes leave closed

**A `Scheme` leaves the unifier that minted it only if it is closed** — every
variable in its body is either ground or bound by the quantifier (or is a
cyclic-binding root, which the ∀-prefix carries as a μ-equation). A scheme that
still mentions a *residual* variable — one free in the minting unifier — must
not cross out of that unifier's lifetime.

This is hard, not stylistic, because quantification is **nominal-by-listing**
over unifier root ids ([[internals/type-inference|type-inference]]): the
quantifier is a `Vec` of `u32`-tagged variable ids, and a free id is meaningful
only relative to the union-find that minted it. A turn's unifier dies with the
turn and the next turn's ids restart at zero. An open scheme imported into a
fresh unifier therefore silently aliases *that* unifier's variables — type
corruption that no check catches, not an error that surfaces.

Closure is enforced where schemes are minted for exit, by generalising against
the empty `TyEnv`: with no environment to subtract, `generalize` quantifies
every residual variable and resolves the body against the final unifier
(`core/src/typecheck/generalize.rs`). Two sites mint:

- the top-level `Bind`-node annotation, where each statement's scheme rides
  the IR into the evaluator (`typecheck::annotate`);
- the alias arm stored on the persistent handler frame at install
  (`typecheck::alias_arm_scheme`).

Every other carrier only transports what those two closed: the build-time
prelude bake harvests the schemes off the annotated prelude's `Bind` nodes
(`typecheck::bake_prelude`), and the serial scope tables carry the installed
`(value, scheme)` pairs across the pipeline-stage helper wire
(`serial::SerialBinding`).

One consequence: α-equivalence is **not** quotiented. `Scheme`'s `PartialEq` is
derived, hence structural on the listed ids, so two schemes minted by different
unifier runs compare unequal even when they denote the same ∀-type. Scheme
comparisons must stay within one unifier run, or go through `fmt_scheme`
(`core/src/typecheck/fmt.rs`), which renames the prefix to canonical Greek
letters before comparing.

See [[decisions/260603_session-scheme-continuity|session-scheme-continuity]] for
the decision this rule serves, and [[internals/type-inference|type-inference]]
for how the prefix is represented.
