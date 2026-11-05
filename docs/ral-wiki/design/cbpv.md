---
generated_at_commit: f2a121e8
---

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
aliases, `eval`, subshells, and trap handlers: a block can be bound, passed,
returned, or installed as an effect handler. Currying desugars to nested
name-binding lambdas, followed by pattern binds for destructuring parameters,
so partial and under-application are uniform. Later parameters shadow earlier
ones, including names introduced by destructuring.

**The checked core is control syntax plus primitives on values.** Each core
constructor corresponds to `Extrinsic/` in Agda; shell forms are a separate
layer ([[invariants/ir-pure-cbpv|the IR invariant]]). Application pushes one
argument frame per argument, then forces the head. Only a lambda terminal
consumes an argument. Returned-thunk application follows the
[[invariants/ir-pure-cbpv|IR invariant]]. A value head is hoisted before its
arguments, as a `let` right-hand side is. A head that returns a function runs
first, so `!{ echo a; return $f } !{ echo b; return 1 }` prints `a` before `b`,
and a failing head runs no argument. A head that is itself a function
computation is bound unrun (Let-fun) and runs when applied, after the
arguments' computations.

**A mechanised core calculus models ral; it is not a refinement of Rust.**
It is Agda (`--safe --without-K`, no postulates): extrinsically typed call-by-push-value over
well-scoped terms, with grades (`F^ε A`, value/output), kinds as head sets, scoped rows up to
permutation, and Hindley–Milner let-polymorphism generalising at `bind` with no value restriction
(proper kinds only). Its CK machine, with an output sink, `capture` and failure, is deterministic
and type-safe (preservation and progress). Unification terminates and is most general; inference
is sound, complete and principal relative to proper kinds, and refuses only untypable terms.
The grade invariant (inferred types pair `output` only with `Unit`) is stated and proved in
`Extrinsic/Infer/Grades`, but that module's bounded check currently exhausts the heap, so it is
not yet validated; `Core.agda` checks that successful inference yields target typing and machine
safety ([[design/types|types]], [[internals/type-inference|type-inference]]).

The calculus contains binding capture and returned-thunk splits; their
insertion is mechanised in components, not yet end-to-end. Equi-recursive types through data,
concurrency and authority are not covered by this core. The correspondence's
standing gaps are listed in
[[decisions/261102_the-checked-ir-is-the-calculus|the checked IR is the calculus]].

An older, monomorphic, intrinsically typed model still covers threads and authority:

- preservation is by indexing; handle-ranking and pool progress rule out waiting cycles;
- round-robin gives an enabled thread an eventual turn, or main-thread finality, not a numerical
  waiting bound;
- workers inherit ambient grants; external requests carry admission evidence;
- cancellation ancestry is separate from handle visibility and result claiming; await/join poll,
  cleanup shields, and gathers drain all accepted children;
- failed captures flush their output before resuming the break; worker output replays on await/join.

Cleanup grace is unbounded, stop causes are collapsed, and bytes are text. Its big-step semantics
has no general adequacy proof. A separate trace semantics gives finite behaviours, capture and
least fixed points of continuous process operators; no adequacy result connects it to either
machine yet. Contextual equivalence and the βη laws are not mechanised.

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
- A thunk value is `⟨M, ρ|occ(M)⟩` by construction: it holds only the session
  bindings its body mentions. The machine's `⟨M, ρ⟩` in focus is a
  *computation closure*, transient and holding the whole environment `M` was
  reached in
  ([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
- Every value is a closure over what it mentions, not a thunk alone: a list,
  record or map literal is `⟨V, ρ|occ(V)⟩` too, formed by `form(val, env,
  sig)` and read apart one layer at a time — `⟨[V₁…Vₙ], ρ⟩`'s *i*th element
  is `⟨Vᵢ, ρ⟩` — a name lent from ρ, a constant built, a nested literal or
  thunk closing over that same ρ rather than restricting again
  ([[internals/evaluator-machine|evaluator-machine]]).
- ρ is a finite map from names to values; Σ, the natives and the frozen
  prelude, is a shell's own constant and is never part of any ρ — a name
  resolves in ρ, then Σ. A `Literal` value closure's inspection reads ρ
  alone, so a name only Σ answers is never one of its entries; `form` checks
  that before choosing the closure over building the literal eagerly.
- **The cost model:** value operations cost O(program text) — occ is
  computed once, where a node is built, and inspecting a literal shares its
  ρ rather than recomputing; only computations cost O(data).

See also [[design/syscalls-are-effects|syscalls-are-effects]] (commands are the effect half this splits off),
[[design/scoping|scoping]], [[design/control-operators|control-operators]],
[[invariants/fixed-arity|fixed-arity]], [[related/system-c|system-c]] (box = thunk, unbox = force),
[[related/call-by-push-value|call-by-push-value]] (the substrate at its source).

**Realised in** [[internals/compilation-ladder|compilation-ladder]] and [[internals/evaluator-machine|evaluator-machine]].

Cite: RATIONALE §"Values and commands", §"One form, one meaning";
`docs/SPEC.md` §2, §5.
