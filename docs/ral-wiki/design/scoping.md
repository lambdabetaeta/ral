# Lexical scoping for data, dynamic scoping for authority

**ral splits scoping by what is being scoped** — lexical for data, dynamic for
authority:

- **Data is lexical.** `let` bindings capture at definition and are immutable, so
  `let x = 1; { let x = 2 }; echo $x` prints `1` and a closure observes the
  bindings in force where it was written.
- **Ambient authority is dynamic.** The working directory, environment overlays,
  capability restrictions, and effect handlers are inherited from the call site
  and scoped by `within` and `grant` over the body's whole dynamic extent.

The duality matches the right model to each:

- data captured at definition is predictable — this is what buys equational
  reasoning and safe `spawn`;
- what files you may touch and which commands exist must be scoped to the call
  site, so that a function defined in an unrestricted context can be invoked
  inside a restricted block and respect that restriction without code changes.

An environment variable follows the dynamic side of this split: it is read as
`!{env}[KEY]`, never as a bare lexical name, and a `within [env: …]` overlay is
seen by `env`, `~`, child processes, and PATH resolution alike
([[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]).

Each dynamic frame nests by its own algebra:

- **capability** frames by intersection — each layer narrows reachability ([[design/grant|grant]]);
- **environment** overlays by shadowing — inner `KEY: VAL` overrides outer;
- **directory** scopes as local state — each saves the one working-directory cell, sets it and restores it on exit, so a `cd` inside moves only the innermost scope's copy ([[decisions/260925_within-dir-is-local-state|within-dir-is-local-state]]);
- **handler** frames by the deep self-masking discipline ([[design/effects-handlers|effects-handlers]]).

A tail-recursive call inside a `within` or `grant` block stays under that scope
across every tail landing.

The active Agda runtime (`dev/agda/CEK/Runtime.agda`) makes the lexical half
explicit: closures contain typed code and lexical environments, with no
dynamic-scope payload. Environment entries hold scheme-indexed values;
instantiation preserves their erased data and captured code. Total typed
value evaluation agrees with an untyped evaluator using positional lookup.
The CEK structural/control rules execute those closures and preserve lexical
capture across caller shadowing. Generalisation evaluates its producer once;
later occurrences instantiate the returned value. Byte feeds save the caller's
remaining stdin and install a closed queue for the body. On return or escape,
the frame restores that remainder without rolling back world state or output.
Checked traces distinguish cleanup inside a feed, which reads its remaining
bytes, from cleanup outside, which reads restored caller stdin. A returned
closure reads the input of its eventual caller. Capture routes stdout through
these typed frames to its nearest buffer; nested failure flushes can therefore
remain inside an enclosing capture.

The active CEK environment rule now validates a sorted text-map overlay,
refuses PWD and OLDPWD before the body runs, saves the caller map, and forces
the supplied thunk under inner shadowing. `CEK.Environment` proves shadowing,
preservation of unshadowed entries, binding provenance and exact reserved-key
validation. On return, error or exit the frame restores the saved environment
without rolling back world state, output or stdin. Commands observe the active
map. Checked traces distinguish cleanup inside/outside a scope and recovery
after restoration; a returned command thunk uses its later caller's map.
`CEK.CommandMachine` proves total entry and immediate return/fail/exit laws
at arbitrary continuations. Forward and reverse capture simulation include
environment entry, refusal and restoration (`CEK/EnvironmentExamples`).

Directory scopes now have matching typed frames. Empty paths raise locally;
`empty-directory-step` characterises every possible first step, excluding a
world call. Nonempty paths use a stateful admission judgment supplied by the
world. Admission saves/sets the opaque path and forces the thunk; refusal
raises before the body. Pop restores only the saved path on every outcome,
retaining admission/body world changes. Checked traces cover nested resolution,
cleanup/recovery, refusal, capture and returned closures using caller directory
(`CEK/DirectoryExamples`). Both capture simulations include these steps.
The world judgment abstracts resolution, permission and existence validation;
its ordered OS/authority refinement remains open. Unbracketed `cd` is outside
this core. File redirects, authority scopes, contextual scope laws and the
dynamic-authority theorem remain open.

Handler scope is now a separate CEK configuration field. Installation evaluates
lexical arms and pushes a user frame; invocation masks exactly its owner and
restores the saved caller scope on return, error or exit. The host base table
is searched after user names and before catch-alls, and its calls run unmasked.
Scope restoration preserves world changes and byte traffic. Checked traces
show deep restoration between calls and returned command thunks using their
later caller's handlers. Both capture simulations preserve the handler
component; contextual scope laws and general handler adequacy remain open.

**A scoping form's body is a thunk the form forces.** `within` and `grant` take
a thunk `U C` — a literal `{ … }`, or a name holding one — and run it inside the
frame they push; `guard`, `try` and `?` take theirs the same way, as `if` and
`case` take their arms. A literal block is forced in place with the current
environment (`Machine::force_val`), closing nothing. An atom that would be
hoisted, and so run before the form chose, is refused at elaboration: bind it
first. The type of the form is the type of the body, so no arm rule is
specific to scoping ([[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]]).

**The checked core laws distinguish returning code from running it.** In
`dev/agda`, `CK.Semantics.Scoping` proves:

- Valid environment overlays and handler installation preserve return and
  halt, distribute over sequencing, recovery and cleanup, and commute with capture in the term
  interpretation, at every ambient scope and world.
- Distribution over recovery or cleanup applies the scope to both the body
  and the recovery/cleanup computation. `ScopedFundamental.EnvironmentControl`
  transfers the environment equations to machine behaviours for covered
  terms under finite ambient layers.
- A thunk returned from a temporary scope and forced afterward runs in the
  caller's scope. `CK.Machine.ScopingLaws` proves exact finite-behaviour
  agreement for this equation under any surrounding stack, for arbitrary
  body syntax. Lexical values remain supplied by substitution.
- Invalid environment entries fail before the body runs. Moving capture
  across that failure changes the model's atomic empty-flush event, although
  the projected channel/outcome views agree; the capture equation therefore
  requires a valid overlay.

Scoped higher-order adequacy accounts for the scope where suspended code is
later invoked; coverage of the whole core remains open.
The checked operational decomposition retains ambient scope while splitting
execution into a body and its continuation (`CK.Machine.DelimitedExecution`).
The body can return a function before a pending application; the surrounding
layers remain installed during its reads and dispatch. The theorem preserves
exact events and accounts for every step, for a nonempty outer boundary that
passes byte channels unchanged, collects no audit entries and contains no live
pipeline stage. Pipelines and channel frames may still occur inside the body.
`CK.Machine.BoundaryExchange` proves that changing such an outer boundary
while preserving its scope leaves all body behaviours and their step counts
unchanged. The continuation may consume the result differently, even at a
different final result type. `DelimitedCompleteness` uses this law to express
body completeness at a scope independently of a particular continuation;
the existing fundamental theorem supplies it at base scope.

`DelimitedScoping` connects body and whole executions under finite scope
layers for computations returning values, including suspended code. Scope
entry, handler dispatch, sequencing and application preserve completeness
under explicit premises about their bodies and returned functions. A checked
example calls a function inside an environment overlay and then reads the
restored caller environment. `ScopedRecursion` supplies finite-execution
induction under those layers: repetition of any independently complete closed body agrees exactly
with its least fixed point, and the silent self-loop denotes bottom. This
recursion result preserves the ambient scope throughout each iteration.

`DelimitedNesting` retains an outer continuation when splitting a body at an
inner boundary, preserving both behaviours and total execution cost. This
supports bind and application completeness at arbitrary computation result
types in `DelimitedSequential`: an intermediate result may itself be a
function.

**Returned code is computable at its invocation scope** in the covered
`ScopedFundamental` fragment. Its logical relation quantifies over compatible
ambient boundaries and proves substitution compatibility for higher-order
CBPV, general recursion, data elimination, codecs, register reads, environment
entry, handler installation, capture, recovery, cleanup and halt. This yields both
directions of finite behaviour agreement at `F A`, including suspended values, without assumptions
about subterm completeness. An escaping thunk reads the restored caller
environment after its creation overlay ends. Capture preserves the same
scope while collecting stdout and flushing on failure. Handler invocation
remains outside this scoped theorem: dispatch needs a computability condition
on selected handler code. The result does not establish whole-core adequacy.

Direct external commands now compose with this higher-order scoped theorem.
An escaping command thunk executes at the caller's scope: leaving its creation
overlay restores the environment and authority used when it is forced.
`DirectExamples` checks both permitted and denied calls, and capture around a
recursive list of commands. The direct-command case requires a semantic world
admitting every response of the current unconstrained machine. This leaves
the arbitrary-world result for command-free code intact; it does not establish
adequacy for a machine constrained to a particular operating system.

## Two layers: lexical scope vs fork inheritance

The lexical scoping above is one mechanism; *crossing a shell boundary* is a
different one, and they should not be conflated.

- **Lexical scope within a shell** splits ρ from Σ. `Env`
  (`core/src/types/env.rs`) is ρ alone: a finite map from names to bindings,
  read with no fallback. `Signature` (`core/src/types/signature.rs`) is Σ —
  the natives and the frozen prelude, constants of the running shell, one per
  shell, `Arc`-shared into every fork. `crate::types::lookup(name, env, sig)`
  is the one resolution rule: ρ, then Σ's prelude, then Σ's natives. A
  binding that shadows a Σ name is an entry of ρ and wins; a name only Σ
  answers is never an entry, so `Env::restrict` never copies it.
  `Env::bind`/`extend` produce a fresh ρ that disturbs no environment a
  closure already captured, so extent is structural rather than a
  pushed-and-popped frame: `M to x. N` closes `N` over the environment the
  `To` frame carries, extended with `x`, and nothing else whatever `M` did
  along the way. ρ is one of two representations, chosen by size and
  invisible to its readers: a sorted array of at most `SMALL` (8) entries, a
  persistent hash map past it; cloning either is O(1) — the allocation is
  shared, not copied — so recursion clones; capture scrubs. A closure holds
  only the bindings its body mentions, `⟨M, ρ|occ(M)⟩`
  ([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
- **Fork inheritance** is [[map/core/shell-state|the flow matrix]] in
  `inherit.rs`: a genuine runtime fork (a `spawn` worker, a pipeline stage,
  a REPL aside, a sub-agent session) clones the parent's *session* `Env`
  whole into the new shell, and its Σ by `Arc` clone, alongside the rest of
  the parent→child manifest (builtin table, dynamic context, cancel root); a
  worker's body brings its own capture in its closure.

A same-thread β-step bridges the two: applying a thunk puts its closure's
`Env` in focus directly — `force(thunk M) = M` pushes nothing — while the
store (`Shell` minus its lexical scope) is shared by identity rather than
forked. A block and a lambda are told apart only by the body's shape
(`Comp::arrow`), never by how force treats them: an unbracketed store write in
either body — `cd`, `alias`, a hook registration — persists past the force: a
plain block is not a boundary for the store. `within [dir:]` / `within
[handlers:]` are the scoped forms, and `within [dir:]` undoes a `cd` made
beneath it
([[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]],
superseding
[[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]
as a description of the mechanism).

See also [[design/syscalls-are-effects|syscalls-are-effects]] (dynamic scope is for the authority over effects), [[design/cbpv|cbpv]], [[design/control-operators|control-operators]].

**Realised in** [[internals/evaluator-machine|evaluator-machine]] (the dynamic frame stack).

Cite: RATIONALE §"Lexical data, dynamic authority"; `docs/SPEC.md` §5, §9.
