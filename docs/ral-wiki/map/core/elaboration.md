---
generated_at_commit: ec5a7b9d
generated_at_date: 2026-10-14
covers_paths: [core/src/elaborator.rs, core/src/elaborator/, core/src/syntax/group.rs]
---

# Map: core / elaboration

`core/src/elaborator.rs` lowers the surface AST into CBPV [[map/core/ir|IR]]. Its
sole public function is
`elaborate(ast, bindings, name) -> Result<Toplevel, ParseError>`: each `let`
becomes a `Phrase::Define`, the members of a recursive group sharing one
`Rec` group, and everything else a `Run`
([[map/core/ir|`Toplevel`/`Phrase`]]).

One `Elaborator` shares state across implementation blocks:

- `elaborator.rs`: entry point, scope and name resolution, hoisting.
- `elaborator/statements.rs`: phrases, nested statements, recursive stretches.
- `elaborator/calls.rs`: head dispatch, application, redirects.
- `elaborator/branches.rs`: conditional arms, short-circuiting, isolated hoists.
- `elaborator/expr.rs`: exhaustive AST traversal and literal lowering.

This is the one phase that knows about surface sugar: it enforces the
value/computation split by binding effectful sub-expressions to fresh temporaries
(threading a *binds* accumulator and folding it into `Comp::Bind` chains at
statement boundaries via `wrap_binds`). No parser syntax survives — the IR the
elaborator hands on carries no surface conveniences
([[invariants/ir-pure-cbpv|ir-pure-cbpv]]).

A temporary is named by `ir::synthetic(tag, n)`, `%var1`, `%eta1`, …: a `%`
never starts a token's identifier, so no source text names a compiler-written
binder and `Elaborator::gensym` needs no skip-bound guard. `ir::is_synthetic`
is the one reader the checker's diagnostics ask, so a temporary never leaks
into an error sentence and a user's own `_variant` is never mistaken for one.
A `let`'s temporaries wrap its right-hand side rather than its `Bind`, since
only the right-hand side reads them.

It also resolves command heads against lexical scope
(`Elaborator::lexical_scopes`), realising the data-vs-authority split of
[[design/scoping|scoping]]:

- a bare name in scope becomes an application of the bound value;
- an unbound bare name becomes `Comp::Exec` against the command namespace;
- `^name`, `./x`, `~/x` heads select external / path / tilde-path dispatch
  directly.

The prelude's exports and the caller's live bindings (REPL env, tool harness) are
pre-loaded into the outermost scope.

`script` is the file the compiling source was read from (`SourceOrigin::file`),
and `$SCRIPT` bakes to it, shown, as a `Val::String` rather than a
`Val::Variable` lookup (`Elaborator::variable_val`), so self-location is
lexical by construction. A virtual source (the REPL, `-c`, a hook, an embedded
`<…>` source) has no file and therefore nothing to bake, and a `$SCRIPT`
reference there is a static error. That is the only way elaboration can fail: the error is parked in one
`Option<ParseError>` slot and checked once at the end, rather than threading
`Result` through every traversal.

`group::parts` (`core/src/syntax/group.rs`) partitions each statement list
into statements and *stretches* of block definitions (SPEC §5.7): any
other statement ends a stretch, and so does a definition of a name the
list has already bound. `Elaborator::elab_stretch` binds all of a
stretch's names, elaborates every member, and takes the references its own
resolution sends to a member (`Elaborator::refer`) as the edges of the
stretch's dependency graph, so the graph cannot disagree with the scoping
it is read from. `group::sccs` (Tarjan) splits it into strongly connected
components, dependencies first: a component of several members, or one
member that refers to itself, becomes one `Rec` group, any other member an
ordinary binding. Only a block or lambda joins a stretch, since binding a
thunk runs nothing; no binding moves across a statement outside its
stretch. A reference from inside a member to a name that a block defines
further down the list, past the stretch, is refused unless the body binds
it above (`Elaborator::refer`). See [[map/core/typecheck|typecheck]] and
[[internals/compilation-ladder|the compilation ladder]] for why recursive
groups are kept monomorphic.

The bound/unbound head decision fixes the IR shape (App vs Exec): a bound head
elaborates to `App`, which the [[map/core/typecheck|typechecker]] resolves
against lexical bindings, builtins, and handlers; an unbound head is `Exec`,
whose typing collapses to the external case (`exec_comp_ty` →
`external_exec_comp_ty`), since a prelude function reaches the checker as a
bound `App` head, never a bare `Exec`.

A pipeline elaborates to a [[map/core/ir|`Pipeline`]] node of its stages and
nothing more: every interior edge is a byte pipe allocated from position.

An arm of `if` and `case` elaborates to a thunk the form forces
(`Elaborator::elab_arm`): a literal `{ … }` or `{ |p| … }`, or a name holding
one. Any other atom would be hoisted and run before the form chose, and is a
parse error; the lone arm of an `else`-less `if` is wrapped as `{ body; () }`
(`elab_arm_unit`)
([[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]]).
