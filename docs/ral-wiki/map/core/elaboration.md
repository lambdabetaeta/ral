---
generated_at_commit: 1cff92ae8c6c493aa045926a8977195c7fb16293
generated_at_date: 2026-07-20
covers_paths: [core/src/elaborator.rs, core/src/syntax/group.rs]
---

# Map: core / elaboration

`core/src/elaborator.rs` lowers the surface AST into CBPV [[map/core/ir|IR]]. Its
sole public function is `elaborate(&ast, bindings) -> Comp`.

This is the one phase that knows about surface sugar: it enforces the
value/computation split by binding effectful sub-expressions to fresh temporaries
(threading a *binds* accumulator and folding it into `Comp::Bind` chains at
statement boundaries via `wrap_binds`). No parser syntax survives — the IR the
elaborator hands on carries no surface conveniences
([[invariants/ir-pure-cbpv|ir-pure-cbpv]]).

It also resolves command heads against lexical scope
(`Elaborator::lexical_scopes`), realising the data-vs-authority split of
[[design/scoping|scoping]]:

- a bare name in scope becomes an application of the bound value;
- an unbound bare name becomes `Comp::Exec` against the command namespace;
- `^name`, `./x`, `~/x` heads select external / path / tilde-path dispatch
  directly.

The prelude's exports and the caller's live bindings (REPL env, tool harness) are
pre-loaded into the outermost scope.

`group::group_stmts` (`core/src/syntax/group.rs`) runs first to find mutually
recursive binding groups. Over a run of adjacent `let`s it builds a dependency
graph — edge `i → j` when binding `i`'s value references name `j`, i.e. dependent
→ dependency — and partitions it into strongly connected components with Tarjan's
algorithm (`find_sccs` / `strongconnect`). A component emits a `LetRec` / `Rec`
exactly when it is recursive (more than one member, or a singleton with a
self-edge) *and* every member is a thunk form; everything else emits plain
`let`s. The thunk-form gate is load-bearing: a non-lambda binding placed in a
`LetRec` would have its body evaluated eagerly against placeholder thunks, turning
a textual forward-reference into a genuine value cycle. Components are emitted in
a topological order (`topo_dfs` post-order) so each dependency is in scope before
the bindings that reference it. Shadowing needs no special split — `resolve_ref`
points each use at the nearest preceding definition, so a rebound name simply
falls into a later component. See [[map/core/typecheck|typecheck]] and
[[internals/compilation-ladder|the compilation ladder]] for why recursive groups
are kept monomorphic.

The bound/unbound head decision fixes the IR shape (App vs Exec): a bound head
elaborates to `App`, which the [[map/core/typecheck|typechecker]] resolves
against lexical bindings, builtins, and handlers; an unbound head is `Exec`,
whose typing collapses to the external case (`exec_comp_ty` →
`external_exec_comp_ty`), since a prelude function reaches the checker as a
bound `App` head, never a bare `Exec`.

A pipeline elaborates to a [[map/core/ir|`Pipeline`]] node whose stage-parallel
arrays the elaborator can only fill with placeholders: a `Wire::EMPTY` per stage
for the byte channel, and a `Ty::Unit` per stage for the value type. Both are
overwritten by the [[map/core/typecheck|annotation pass]], which writes each
stage's resolved mode-wire and value type onto the node's `wires` and
`stage_types` fields once it has typed the pipeline. The evaluator never reads
either array on an un-annotated comp, so the placeholders are harmless; the
retained per-stage value types feed the structural REPL's typed spine.
