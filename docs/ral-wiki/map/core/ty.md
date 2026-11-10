---
generated_at_commit: 64b60eafa2eec1a958ff29d8671376d79856dc3a
generated_at_date: 2026-11-10
covers_paths: [core/src/ty.rs, core/src/ty/]
---

# Map: core / ty

`core/src/ty.rs` and `core/src/ty/` are the type language as data, below the
[[map/core/ir|IR]] and the [[map/core/typecheck|checker]]: the IR's schemes and
sites name it, and nothing in it reads a unifier or a runtime value.

- `ty.rs`: `Ty`, `CompTy`, `Grade`, `Row`, `Label`, the unification-variable
  newtypes, `Var` (a variable of any sort) and `Term` (a term of any sort);
  `TAG_PREFIX`, the sigil of a tag, written once.
- `ty/kind.rs`: `Kind`, the closed set of predicates a type variable carries
  (`number`, `comparable`, `scalar`, `sized`, `data`), their meet, and what a
  head is.
- `ty/scheme.rs`: `Scheme { ty, tys, comps, rows, grades }`, one map per sort
  from variable to `Role<M, B>::{Forall(M), Weak(M), Mu(B)}`. `M` is what
  minting a fresh variable of that sort needs (a type's `Kind`, a row's deep
  bit), `B` the sort's μ-binding: `Ty`, `CompTy`, and the uninhabited `NoMu`
  for rows, since a cycle through a row is rooted at a type or computation.
  Grades are a set, only ever quantified. A role is thus of its variable's
  sort by construction. `Weak` records a rank-zero residual for reseeding;
  ranks themselves live in the checker. `Closed` is a scheme writing no variable it
  gives no role, constructed only by settlement, `Binders::close` (a builtin's
  own binder supply) or checked deserialisation. Equality is structural.
  There are no partial schemes or cached environment free variables
  ([[invariants/schemes-leave-closed|schemes leave closed]]).
- `ty/exec_arg.rs`: `RefusedArg`, the shapes `execve(2)` has no argument for, one
  match on a `Head`.
- `ty/typed.rs`: `Typed`, a `Datum` whose encoding has a ral type derived from
  its own fields, and the record and variant types ral shows for a Rust type
  ([[map/core/transport|transport]]).
- `ty/fmt.rs`: `Display` for `Ty` and `Scheme`; one `FmtCtx` names a shared
  variable the same in every type a diagnostic prints. Producers print as
  `Command`, `Returns A` or `Command[ν] A`; a free grade paired with a type
  excluding unit prints `Returns A`, using quantified kinds where known.
  Scheme quantifiers list only visible grades. Local unit tests build schemes
  directly to check quantified computation variables and cyclic roots.
- `ty/site.rs`: the data of a `Site`, a boundary's checked type frozen as a graph
  of shared nodes (`Arc<[Node]>`, `Shape`, `Fields`, `Tail`) naming its unit by a `UnitId`
  — an OS nonce drawn once per process and a count, so unique in every process
  that meets it, a baked prelude unit's included. `Fixings` is the session's
  table of what each unit's weak `comparable` variables were fixed to, keyed by
  `(UnitId, var)`, which a session's threads share and
  a new session snapshots ([[map/core/shell-state|shell-state]]). The checker
  builds a `Site` (`typecheck/site.rs`), one fresh `UnitId` per unit, and the
  runtime walks it against the running session's table (`shell/runtime/admit.rs`).
  Free type/computation nodes, open row tails and free grades retain canonical
  ids; the first three also retain rank-zero weakness. Projections share the graph
  and unit while selecting another root. Reinstantiation freshens each identity once, preserving
  sharing, cycles, kinds and grades; quantified `comparable` parameters do not
  acquire session fixings. `Site::scheme` enters a scratch region before
  instantiation, exits, then generalises and settles a closed contract for delayed
  protection wrappers ([[map/core/evaluator|evaluator]]).

`Str`, the shared immutable string a `Value::String` holds, lives in
`core/src/data/string.rs`, exported as `data::Str`.
