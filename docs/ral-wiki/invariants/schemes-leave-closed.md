# Schemes leave closed

**A `Scheme` leaves the unifier that minted it only as a `Closed`.** A closed
scheme writes no variable it gives no role: every variable in its body and
μ-bindings is quantified (`Role::Forall`, or a member of its grade set), a
cyclic-binding root (`Role::Mu`), or a listed *weak* residual (`Role::Weak`,
below). An unlisted free variable must not cross out of the minting unifier's
lifetime.

This is hard, not stylistic, because quantification is **nominal** over
unifier root ids ([[internals/type-inference|type-inference]]): a scheme's
roles are keyed by `u32`-tagged variable ids, and a free id is meaningful only
relative to the union-find that minted it. A run's unifier dies with the run
and the next run's ids restart at zero. An open scheme imported into a fresh
unifier can therefore silently alias *that* unifier's allocated variables;
nothing at the import can tell a stale id from a live one.

**Closedness is a type.** `Closed` (`core/src/ty/scheme.rs`) has three
constructors, each checking `Scheme::foreign` is empty:

- settlement, `settle_weak` (`core/src/typecheck/generalize.rs`), run on a
  successful unit's `Solution`;
- `Binders::close`, a builtin's own binder supply, drawn from zero and
  borrowing no inference store;
- deserialisation, which refuses a scheme that is not closed.

Every carrier past a unit holds `Closed`: `Phrase::Define`'s schemes in a
checked `Toplevel`, `SessionSchemes`, the build-time prelude bake (read back by
`SessionSchemes::from_prelude`) and the seed's scope tables
(`seed::table::SerialBinding`). Live schemes exist only inside a unit:

- Generation harvests rank-generalised per-name schemes onto the pending
  `Phrase::Define`. Only after successful unit closure does `Readback::toplevel`
  settle them.
- One-shot scheme builders (`alias_arm_scheme`, `binding_value_scheme`) enter a
  region before checking the body, exit before generalising at rank zero, then
  close the unit. Only success settles and exports the scheme; closure errors
  export nothing. `catch_all_stands_in` stores no scheme.
- `Site::scheme` enters its scratch region before instantiation, then exits,
  generalises and settles the contract.

**Sortedness is structural.** A scheme keeps one map per sort, so a role is
always of its variable's sort. A type role's metadata is its `Kind`, a row's
its deep bit; a row's μ-binding is uninhabited, since every cycle is rooted at
a type or computation; grades are a set, never weak and never μ-bound.

**No suspended match survives the generalisation of its scrutinee.** A successful
region defaults or discharges all young matches; an old match lowers everything
it mentions to its scrutinee's rank. Thus no match can later modify quantified
unknowns, and schemes contain no partial constraints
([[decisions/261110_elaboration-is-decided-by-suspended-matches|suspended matches]]).

A weak residual is the variable a stored scheme may carry unquantified:
rank zero prevents any `let` from quantifying it. A `Role::Weak` entry records
a value, computation or row identity; `Solver::seed` re-seeds each as a fresh
rank-zero unknown of the next unifier, preserving kinds and row deep bits so a
residual retains its restrictions. A `Define` scheme is settled before it is
stored, so no foreign id is ever read
([[decisions/260930_a-let-generalises-what-is-not-weak|a-let-generalises-what-is-not-weak]]).

Closure is syntactic over the **whole scheme graph**: the body and every
value/computation μ-binding. It must not follow the checking unifier's slots,
which could hide a dangling id by resolving it to ground. Settlement substitutes
live weak-variable images and resolves residual grades in every equation, not
only in the body, even with no weak variables; local ∀/μ names remain opaque.
It quantifies remaining free grades. Reseeding alpha-renames all local binders
as well as minting weak residuals, simultaneously, into disjoint fresh names.
Otherwise a fresh weak id could equal an old binder and be captured on
instantiation.

One consequence: α-equivalence is **not** quotiented. `Scheme`'s `PartialEq` is
derived, hence structural on the ids, so two schemes minted by unrelated
unifier runs compare unequal even when they denote the same ∀-type. Scheme
comparisons must stay within one unifier run or go through `Scheme`'s
`Display` (`core/src/ty/fmt.rs`), which renames the prefix to canonical Greek
letters. Raw variable ids are not canonical: re-checking a checked tree need
not reproduce them.

See [[decisions/260603_session-scheme-continuity|session-scheme-continuity]] for
the decision this rule serves, and [[internals/type-inference|type-inference]]
for how the roles are represented.
