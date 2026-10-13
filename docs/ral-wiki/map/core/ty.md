---
generated_at_commit: 446e3123
generated_at_date: 2026-10-06
covers_paths: [core/src/ty.rs, core/src/ty/]
---

# Map: core / ty

`core/src/ty.rs` and `core/src/ty/` are the type language as data, below the
[[map/core/ir|IR]] and the [[map/core/typecheck|checker]]: the IR's schemes and
sites name it, and nothing in it reads a unifier or a runtime value.

- `ty.rs`: `Ty`, `CompTy`, `Grade`, `Row`, `Label` and the unification-variable
  newtypes; `TAG_PREFIX`, the sigil of a tag, written once.
- `ty/kind.rs`: `Kind`, the closed set of predicates a type variable carries
  (`number`, `comparable`, `scalar`, `sized`, `data`), their meet, and what a
  head is.
- `ty/scheme.rs`: `Scheme`, a type under quantifiers, and the weak-variable
  sets a scheme did not quantify.
- `ty/exec_arg.rs`: `RefusedArg`, the shapes `execve(2)` has no argument for, one
  match on a `Head`.
- `ty/fmt.rs`: `Display` for `Ty` and `Scheme`; one `FmtCtx` names a shared
  variable the same in every type a diagnostic prints.
- `ty/site.rs`: the data of a `Site`, a boundary's checked type frozen as a graph
  of nodes (`Node`, `Shape`, `Fields`, `Tail`) naming its unit by a `UnitId`
  — an OS nonce drawn once per process and a count, so unique in every process
  that meets it, a baked prelude unit's included. `Fixings` is not in the
  code: it is the session's table of what each unit's `comparable` variables
  were fixed to, keyed by `(UnitId, var)`, which a session's threads share and
  a new session snapshots ([[map/core/shell-state|shell-state]]). The checker
  builds a `Site` (`typecheck/site.rs`), one fresh `UnitId` per unit, and the
  runtime walks it against the running session's table (`types/admit.rs`).

`Str`, the shared immutable string a `Value::String` holds, lives in
`core/src/text.rs`.
