---
generated_at_commit: 7a4bf71e
generated_at_date: 2026-08-25
covers_paths: [core/src/elaborator.rs, core/src/syntax/group.rs]
---

# Map: core / elaboration

`core/src/elaborator.rs` lowers the surface AST into CBPV [[map/core/ir|IR]]. Its
sole public function is
`elaborate(ast, bindings, name) -> Result<Comp, ParseError>`.

This is the one phase that knows about surface sugar: it enforces the
value/computation split by binding effectful sub-expressions to fresh temporaries
(threading a *binds* accumulator and folding it into `Comp::Bind` chains at
statement boundaries via `wrap_binds`). No parser syntax survives — the IR the
elaborator hands on carries no surface conveniences
([[invariants/ir-pure-cbpv|ir-pure-cbpv]]).

A temporary's extent is bounded by two mechanisms, and both are load-bearing.
`wrap_binds` wraps the chain in a `ScopeOp::Hoisted` frame, so the temporaries
die with the computation that reads them; nothing else pops them, and a
top-level `Bind` installs into the session scope, so without the frame a
temporary stayed readable as `$_var1`, was PATH-shadow-checked on the way in,
and was harvested by the binding-lease ledger. A `let`'s temporaries wrap its
right-hand side rather than its `Bind`, since only the right-hand side reads
them and a frame around the `Bind` would take the user's own binding down with
it. `Elaborator::gensym` then skips any name already bound: `_` is ral's
internal namespace — `use` hides it, the `_ed-*` builtins live in it — not an
unwritable one, so a temporary must never capture a user's own `_var2`.

It also resolves command heads against lexical scope
(`Elaborator::lexical_scopes`), realising the data-vs-authority split of
[[design/scoping|scoping]]:

- a bare name in scope becomes an application of the bound value;
- an unbound bare name becomes `Comp::Exec` against the command namespace;
- `^name`, `./x`, `~/x` heads select external / path / tilde-path dispatch
  directly.

The prelude's exports and the caller's live bindings (REPL env, tool harness) are
pre-loaded into the outermost scope.

`name` is the compiling source's display name, and `$SCRIPT` bakes to it as a
`Val::String` rather than a `Val::Variable` lookup (`Elaborator::variable_val`),
so self-location is lexical by construction. A source with no script identity
(`path::lex::has_script_identity` — the REPL, `-c`, a synthetic `<…>` source)
therefore has nothing to bake, and a `$SCRIPT` reference there is a static
error. That is the only way elaboration can fail: the error is parked in one
`Option<ParseError>` slot and checked once at the end, rather than threading
`Result` through every traversal.

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

A pipeline elaborates to a [[map/core/ir|`Pipeline`]] node carrying two
annotations the elaborator can only fill with placeholders: a `Ty::Unit` per
stage for the value type, and a `PipeYield::Last` for the node's yield. Both are
overwritten by the [[map/core/typecheck|annotation pass]] once it has typed the
pipeline. The evaluator never reads `stage_types`, which feeds the structural
REPL's typed spine; it reads the yield only to decide whether the last stage's
helper reports a value, and the checker runs before every evaluation, so the
placeholder is never observed.
