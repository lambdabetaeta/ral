# Call-by-push-value: values and commands

**ral's organising principle is the call-by-push-value separation of *values*
from *computations*:**

- *values* are inert data: strings, integers, lists, records, thunks;
- *computations* read, write, fail, return a value, or await an argument;
- a thunk `{M}` suspends a computation as a value; `!` forces it back into a
  computation: `force(thunk M) = M`, one thunk value and no bracket
  around the body ([[design/scoping|scoping]]).

The typed calculus is ordinary call-by-push-value ([[design/types|types]]). A
command is a computation `F^w Unit`: its value is its output, and `F` carries a
grade saying so. Every form
that suspends a command — `if`, `case`, `try`, `guard`, `within`, `grant` —
takes a thunk and forces the one it chooses.

Two sigils keep retrieval and forcing distinct:

- `$name` dereferences a binding to its value without side effect;
- a bare word in head position is looked up and, if it names a thunk, forced.

**Once captured, data is never re-lexed, split, or globbed.** So the shell
collapse — every datum at once a string, a command name, and re-lexable
source — never arises, and neither do the escaping bugs that follow from it.

The parameterised block `{ |x| M }` is the language's single abstraction
mechanism. It subsumes what conventional shells splinter across functions,
command interceptors, `eval`, subshells, and trap handlers: a block can be bound, passed,
returned, or installed as an effect handler. Currying desugars to nested
name-binding lambdas, followed by pattern binds for destructuring parameters,
so partial and under-application are uniform. Later parameters shadow earlier
ones, including names introduced by destructuring.

**The checked core is control syntax plus primitives on values.** Shell forms
are a separate layer ([[invariants/ir-pure-cbpv|the IR invariant]]). Application
pushes one argument frame per argument, then evaluates its computation head.
A λ consumes an argument. A builtin forces to its η-expansion: ordinary λs
bind arguments before δ invokes the captured entry. Returned-thunk
application follows the [[invariants/ir-pure-cbpv|IR invariant]]. A value head
is hoisted before its arguments, as a `let` right-hand side is. A head that
returns a function runs first, so
`!{ echo a; return $f } !{ echo b; return 1 }` prints `a` before `b`,
and a failing head runs no argument. This holds for a function computation too:
a `let` always runs its right-hand side. A λ that finds no argument returns
itself as a block, restricted to the names it mentions and holding the
arguments already supplied.

Bindings are immutable. Re-`let` introduces a fresh binding that shadows the
outer one within its lexical scope; closures capture at definition time. This
buys equational reasoning in the pure fragment and makes `spawn` safe — a
spawned block shares nothing mutable.

**Invariants.**
- `$` consults only the value namespace and never triggers command lookup or
  handler dispatch.
- Captured external output is decoded UTF-8 *strictly* — invalid bytes fail
  with a hint to keep them via `| from-bytes` — and the decoded text is never
  re-lexed.
- Closures observe the bindings in force where they were defined, not where they
  run.
- Every runtime value is a *value closure* `⟨V,ρ⟩`, with nonvariable root
  and every direct name bound in ρ. Scalar, byte-string, builtin and handle
  constructors use empty captures; thunk formation gives `⟨thunk M,ρ↾occ(M)⟩`.
  A machine computation closure `⟨M,ρ⟩` instead holds the whole environment
  in which it is reached.
- `ValClos::close` restricts at formation and pulls Σ's answers for missing
  direct names into ρ. A name under a thunk is not direct: it resolves ρ
  then Σ when the thunk runs. Σ is the shell's fixed builtin and prelude
  signature, not a captured mutable store.
- Reading a component shares the saved environment: a variable `x` is its
  binding `ρ(x)`; otherwise the component V′ is `⟨V′,ρ⟩`. No re-restriction
  or Σ lookup occurs, and a head reads no deeper than its own constructor.
  Lists and maps built at run time are ordinary syntax over fresh variables
  bound in ρ, held as persistent vectors and ordered maps
  ([[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]],
  [[internals/evaluator-machine|evaluator-machine]]).
- Machine state is a computation closure or `Err(Break)` only. A produced
  value passes to frames through `settle` in the same step; it is never
  stored as control or wrapped in newly constructed `return` syntax. See
  [[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]].
- **The cost model:** collection nodes and captures are shared. Cloning a
  runtime root tag spine costs O(n); a syntax-component read may clone its tag
  suffix. Whole-value traversals peel consecutive tags rather than repeatedly
  materialising suffixes.

See also [[design/exec|exec]] (commands are the effect half this splits off),
[[design/scoping|scoping]], [[design/control-operators|control-operators]],
[[invariants/fixed-arity|fixed-arity]], [[related/system-c|system-c]] (box = thunk, unbox = force),
[[related/call-by-push-value|call-by-push-value]] (the substrate at its source).

**Realised in** [[internals/compilation-ladder|compilation-ladder]] and [[internals/evaluator-machine|evaluator-machine]].

Cite: RATIONALE §"Values and commands", §"One form, one meaning";
`docs/SPEC.md` §2, §5.
