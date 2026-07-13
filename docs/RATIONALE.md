# ral — design rationale

This document is about why ral is the way it is.  The specification says
what ral is.  Here we record, for each design choice, the reasoning behind
it and the cost it carries — motivation, not contract.  The spec is normative
where the two disagree.

## Influences

- **rc** (Plan 9) and **es** (Haahr–Rakitzis): lists and functions as
  values; control structures in a library, not the grammar.
- **Algol 60**, **Modernised Algol**: block structure, lexical scoping,
  and the distinction between a value and a computation producing one.
- **Call-by-push-value** (Levy): a formal calculus in which that
  distinction is primitive, with thunks first-class.
- **Haskell**, **Backus**: immutable bindings, combinators, equational
  reasoning in the pure fragment.
- **Tcl**: commands are ordinary names, resolved at evaluation time,
  not keywords.
- **Shill** (Moore et al.) and capability systems: authority is
  explicitly delegated and may be attenuated, never amplified.
- **JavaScript**, **Rust**: destructuring, closures, spread, string
  interpolation.

The closest relatives are **YSH/Oil** and **nushell**.  YSH/Oil retains
POSIX compatibility.  Nushell enforces structured data on every pipeline
stage.  Ours differs by doing neither: we retain no POSIX compatibility,
and we do not require structure at every stage.

## Roadmap

ral rests on one overarching identification, from which the dynamic design
follows, together with seven further commitments that shape the surface and
the runtime.  The identification is that **system calls are algebraic
effects** — running an external command is the performance of an operation
whose interpretation is supplied separately, by default the operating
system.  From there the tour proceeds.  We separate *values* from *commands*,
the inert from the effectful, so that captured output is never re-lexed.  We
make every binding immutable, so that a name's meaning is fixed once it is
bound and a `spawn` is safe without synchronisation.  We let a program supply
its own interpretation of an operation through *scoped effect handlers*, the
reinterpretation layer the identification leaves open.  We confine authority
through *explicit sandboxing*, a capability wrapper whose braces are
lexically apparent and whose restriction reaches dynamically into every
callee.  We give the pipe *two execution models*, one for values and one for
bytes, settled by the type of the connecting edge.  We make *handlers and
aliases variadic while functions are not*, because the asymmetry is the
exec/lambda boundary made visible.  And we keep *a spare surface*, two sigils
and no hidden lexer modes, refusing to overload one form with two meanings.
Eight commitments in all, threaded in that order through the sections below.

## System calls are algebraic effects

The value/command distinction leaves one question open: what *is* a command?  Our answer is the identification on which the whole of ral's dynamic design turns, and it is worth stating plainly before any of its consequences.  **Running an external command is the performance of an *algebraic effect operation*.**  A name such as `git` or `cat`, which the binding namespace declines — neither a lexical `let`, nor a builtin, nor a prelude name — is an *operation*: it is performed, it returns once with a result or a failure, and its meaning is supplied separately by its *interpretation*, by default the operating system carrying out the syscall.  Separating the operation from its interpretation is the defining move; everything dynamic in ral rests on it.

The identification is generous: from it alone, with no further machinery, the rest of the dynamic design follows.  First, authority is permission over the effect set — a capability narrows *which* operations may be performed and on what arguments, which is exactly what `grant` does.  Second, the capability check sits at the effect-performance site: ral vets the call against the active grant lattice at the very point it would dispatch the operation, before it resolves an executable image at all.  Third, audit is a trace of operations performed and the scopes that framed them.  Fourth, failure is an operation's exceptional outcome — a command returns, emits bytes, or *fails*, and failure is the effect's outcome, not a `Bool`.

The reader's natural objection is that *every* builtin which touches the kernel ought then to be an effect.  We meet it head-on: not every kernel call is an effect.  A structured-query builtin such as `list-dir` or `file-info` reaches the kernel — it calls `read_dir`, it `lstat`s a path — yet it is deliberately pulled to the value side, answering with a record rather than bytes, precisely so that the capture-then-reparse round-trip never arises.  What makes a name an operation is being an *open external name* whose meaning comes from its interpretation, not the mere fact of touching the OS.  Hence we draw the effect boundary at the external-*process* surface — the `exec`/`wait` family that defines a shell.

Two further choices are *orthogonal* to the principle, and we defer both to *Scoped effect handlers* below, taking on each the minimal setting.  The first is whether a program may *reinterpret* an operation rather than defer to the OS; the second, should it reinterpret, whether the reinterpreting handler may capture the continuation.  The principle stands without either — an external command is an effect whether or not any handler intercepts it.  And performing one needs no reified continuation: a syscall is a single forward step into the world, so the continuation is the ordinary rest of the computation, never a value the program captures.

## Values and commands

This section is about the one distinction from which the rest of ral's surface follows: the separation of *values* from *commands*. A value is inert data — a string, an integer, a list, a record, a stored block — that may be named, passed, and inspected but never, of itself, executes. A command is an effectful process that may read input, emit bytes, return a value, or fail. Most shells collapse the two. Every datum is a string, and every string is at once data, a command name, and source text for further evaluation; the consequence is that captured output is re-lexed, split on whitespace, and glob-expanded before the next stage ever sees it. We refuse this collapse, and thereby avoid the entire class of quoting and word-splitting bugs that grows from it — yet without surrendering first-class commands and pipes, the two things a shell exists to provide.

The formal account is call-by-push-value (Levy). Values are inert; computations are effectful; a *thunk* packages a computation as a value; and forcing runs it. One need not know the theory to use the shell. The slogan is that **a value *is* and a computation *does***, and at the surface a block `{M}` thunks while `!` forces.

The crossing between the two worlds is deliberate, and it is the single abstraction mechanism. A block `{M}` is a command packaged as a value — a thunk — and `!` forces it back into running; `{ |x| M }` is a function, the same thunk awaiting arguments. This one mechanism does the work that conventional shells splinter across five: functions, aliases, `eval`, subshells, and trap handlers all become a block that is bound, passed, returned, or installed. A block can be stored in a `let`, handed to `map`, or installed as an effect handler, and it is the same object throughout.

Forcing is always explicit, with a single exception — a bound name resolved in head position is forced, or applied, implicitly. This is what keeps ordinary calls reading naturally, `greet alice`, while the storage of a command stays visible, `let plan = { make build }`: the braces mark, at a glance, that no work has yet happened. The natural objection is that two rules are harder than one; the answer is that the implicit case is exactly the one where a human reads an application and means an application, so the convenience never hides an effect. And because the value a captured command returns is data, it is never re-lexed, split, or globbed — the capture-then-reparse round-trip, the original sin of the string-only shell, simply has no place to occur. Concretely, capturing a command's stdout in a `let` decodes it as UTF-8, strips one trailing newline, and binds a `String`; the decode is *strict*, so invalid bytes fail with a hint to keep them via `| from-bytes` rather than quietly substituting replacement characters.

## Immutable bindings: shadowing, not mutation

This section is about why ral has no mutable variables.  Every `let`
binding is immutable, and a re-`let` does not overwrite the old name — it
introduces a fresh binding that *shadows* the prior one within the
enclosing lexical scope.  The slogan, to borrow the shape of "a value
is, a computation does", is that **a name's meaning is fixed once it is
bound**.  Closures inherit the same discipline: a `{...}` block captures
its environment at the point of definition, not at the point of forcing,
so it observes the bindings in force where it was written rather than
wherever it later runs.

The reader's natural objection is that this forbids the mutable
accumulator — the running total a loop body bumps on each pass — and that
objection is correct.  We meet it head-on: there is no such accumulator,
and we do not regret its absence.  Iteration that needs to carry state
threads it explicitly through `fold`, through `reduce` (which is `fold`
without an initial value), and, for byte streams, through the streaming
`fold-lines`.  The state becomes an argument and a result rather than a
cell written behind the loop's back — the same move by which functional
programming has long replaced assignment, and no less expressive for it.

The benefit is twofold.  First, local reasoning is easier.  When a name
cannot be reassigned, reading code never requires tracing every prior
statement to learn what `$x` now holds; its meaning is its defining
right-hand side, and equational reasoning holds in the pure fragment.
Second, and less obviously, `spawn` is safe without a line of
synchronisation.  A spawned block runs on its own thread over a *copy* of
the captured environment, and because nothing in that environment can be
mutated, the parent and the worker share no cell that two threads could
race on — the only channel between them is `await`.

This is the same property the process side of *Two pipeline models*
relies on: a stage of a byte pipeline runs out-of-process and is
therefore a *subshell with respect to mutation*, its `cd` or `env-set`
never flowing back.  Under immutability the in-process value pipeline and
the out-of-process byte pipeline agree — neither lets a stage scribble on
its neighbours — so the two models differ only in where they run, not in
what they may touch.  Finally, the surface pays nothing for any of this.
*A spare surface* wants no machinery for re-assignment: there
is no `set`, no `=` as a statement, no distinction between declaration
and update.  A name is bound by `let` and shadowed by `let`, and that is
the whole story.

## Scoped effect handlers

This section is about the orthogonal reinterpretation layer promised by *System calls are algebraic effects*: the means by which a program supplies its own interpretation of an operation in place of the OS default. The identification leaves the door open; **a handler is what walks through it.** `within [handlers: [git: H], handler: K] { body }` installs interpretations on a dynamic stack for the dynamic extent of `body`, and the matched entry runs instead of the syscall. Handlers are *additive*: the effect identity holds whether or not any handler intercepts, so a program reads the same way under a mock as it does against the real OS.

The first design decision is where handlers sit in name resolution, and the answer is *last*. A handler fires only after the binding namespace has declined the name — resolution runs environment, then builtins, then handlers, then the external surface. Hence a handler can mock `git` or `cat`, but it cannot replace `length`, which is a builtin; and a local `let foo` beats an active `foo` handler. Ordinary language names thus stay stable, and command mocking is confined to the open external names where it belongs.

Two further questions, each conflated under one heading in the literature, get a definite answer. *Deep versus shallow* asks whether a handler persists across the continuation of the operation it handles. After `within [handlers: [git: H]] { git a; git b }`, both calls trigger `H` — the frame is not consumed at first use — so by the Plotkin–Pretnar criterion ours are deep. *Self-masking versus self-transparent* asks whether `H` is still in scope during its own body. In ral the matched frame is lifted off the stack for the dynamic extent of the body, so a call to the same name from inside `H` reaches the next outer frame, or the OS, never `H` itself; ours are self-masking.

The reader may object that self-masking is a peculiar default. It is not, and the reason is that we have no first-class `resume` — a handler receives the name and arguments and returns a value or fails. In the Plotkin–Pretnar calculus self-re-entry is harmless because all re-entry flows through `resume k`, and `k` runs under `H` by construction; the continuation discipline does the work. Lacking that machinery, self-masking is the operational rule that keeps the dominant wrap-and-forward idiom — `within [handlers: [git: { |args| my-git ...$args }]]` — free of infinite recursion, without writing `^git` in every body. The shell intuition is exactly a POSIX function named `git` that calls the real `git`: the name shadows only *outside* itself. The `^name` form is an explicit bypass of the lexical/prelude/builtin chain — handlers still apply, because `^name` is a flag on the lookup, not a frame-unwind.

A corollary closes the layer. Control flow is library; exactly five operators — `within`, `grant`, `try`, `guard`, `audit` — earn a grammar arm, by a two-part criterion: a typing rule not derivable from ordinary Hindley–Milner over the builtin signature, *and* runtime semantics not expressible as a function taking thunks. Handler installation is one such case, which is why `within` is a keyword and not a prelude name.

## Explicit sandboxing

This section is about confinement.  A shell that can run anything is a shell that can destroy anything, and the only honest defence is to make the bound on a program's authority visible at the call site, real at the kernel, and impossible to slip past.  ral's answer is `grant`.  Unlike a setuid wrapper or a daemon that vets requests at arm's length, `grant` is *a capability wrapper around a block*: `grant [exec: …, fs: …, net: …] { body }` narrows the active authority for the duration of `body` and otherwise composes like any other block-bodied builtin.  Capabilities are attenuable, never amplified — the lineage is Shill — so a dimension a grant omits inherits the ambient authority, a dimension it names can only narrow it, nested grants compose by meet, and a deny is anti-monotonic: a later layer adds denials but never reopens a denied region.

The intuition is that **the braces are lexically apparent, but the restriction is dynamically reaching** — you can see the `grant` braces, yet the narrowed authority flows into every callee, including a function defined in another module.  The body itself evaluates locally, in the caller's process; the evaluator merely intersects authority onto a dynamic stack.  What an `fs`- or `net`-restricting grant adds, on a platform with OS sandboxing, is *per-command child confinement*: each external or bundled command spawned inside the block is launched under the effective platform sandbox — Seatbelt on macOS, `bwrap` on Linux, an AppContainer on Windows — while ral's own dispatched effects are gated in-process before the syscall.  These are two enforcers, not one mechanism described twice, because each is authoritative exactly where the other is blind.  An OS sandbox confines only a child and never sees an operation ral performs itself; the in-process gate sees ral's own dispatch but cannot follow a child once it runs.

Three points make the discipline concrete.

First, *the dangerous verb wears its name*.  Destruction is never abstracted: removing a tree is written `rm -rf`, the recursing flag visible at the call site, written on purpose by a caller who can see what they typed.  A polite wrapper that decides for itself whether to recurse would hide exactly the decision a shell must keep visible.  Effects therefore go through the bundled coreutils, where the destructive flag is part of the name — there is no `remove-file` primitive to launder it through.

Second, *no back door*.  One might fear that reaching for `cp` rather than a structured primitive escapes the cage; it does not.  The chokepoint is the exec lattice, not a re-parsing of paths: before any external or bundled command spawns, the full `(head, args)` call clears the active grant through `check_exec_args`, a three-valued decision (allow, allow-these-subcommands, deny) that the orthodox object-capability model cannot express — a base profile can veto a name a restrict file never mentions.  A bundled coreutil's own filesystem reads and writes, by contrast, happen inside upstream uutils code that does not call ral's `check_fs_op`.  Reaching for `cp` is therefore not gated by re-deriving its source and destination — it is gated by refusing the inline placement under any active projection and spawning the tool as a confined child, so its self-issued I/O is held by the same kernel sandbox an external would receive.  The structured primitives and redirects, which ral *does* perform itself, are the operations gated in-process by `check_fs_read` / `check_fs_write`.  Either way both roads end at one fence; what differs is which enforcer mans it.

Third, *scope reaches into relative paths*.  A `within [dir: …]` inside a grant cannot escape the enclosing policy, because a path is canonicalised after resolution against the scoped working directory — `.` and `..` collapsed, symlinks resolved — and only the resolved path is matched.  The scoped cwd is not installed by mutating the process: a logical cwd is threaded into each spawn instead, precisely so that a `cd` in one thread cannot race a sibling.  The cost is paid honestly.  `net` has no in-process gate at all, since ral dispatches no network operation for a gate to see, so on a platform without a sandbox backend a `net`-restricting grant fails closed rather than running unconfined — and on Linux `bwrap` cannot path-filter a child's own re-execs, a known seam we name rather than paper over.  This is the boundary exarch reuses unchanged: an agent turn is a host-pushed grant frame over the very same stack.

## Two pipeline models

A pipeline `|` has two distinct execution models, and the type of the adjacent edges decides which one runs. The intuition is that the pipe is whatever its data demand it to be: a pipeline that only ever threads values needs no operating system at all, whereas a pipeline that touches bytes is the genuine Unix article. **The connecting edge's type, settled by the checker, is what selects the model — not any keyword and not any runtime guess.**

The first model is the *value pipeline*. When every stage operates on the value channel, `x | f` is nothing more than typed data-last composition: it reduces to `f !{x}`, evaluated sequentially in the parent evaluator. No process is spawned, no pipe exists, and no process group is formed. Thus `range 1 21 | filter $even | sum` is three function calls threaded by the value channel — the same β-rule `x | f = f !{x}` realised once, parent-side.

The second model is the *byte pipeline*. As soon as one edge touches bytes — at least one external command, or any byte edge — the whole pipeline runs as a Unix-style process pipeline. Three facts hold together. First, every stage, *including* ral-implemented ones, executes in a subprocess. Second, all subprocesses share one process group. Third, the parent ral process is *not* a member of that group: the children call `setpgid` in their own `pre_exec`, and ral never joins. Keeping the parent out is precisely what lets job control stay coherent — a shell that participates in its own foreground pipeline cannot consistently both own the terminal and not own it, since terminal ownership is a property of a single process group that the shell must hand away and reclaim.

The reader will object that this isolation has a price, and it does — we state it plainly. A ral stage running out-of-process is a *subshell with respect to mutation*: its `cd`, `env-set`, alias, module, registry, or REPL changes do not flow back to the parent; only the pipe contents and the final value cross the boundary. This is acceptable because it is exactly how every traditional shell treats process-pipeline stages, and because the alternative — smuggling mutations back across a process boundary — would forfeit the very isolation that makes the byte model honest. The platform mechanics that realise the shape — the process group and `tcsetpgrp` handoff, the exec trampoline that wins the foreground-claim race, the helper-frame protocol, and the Windows Job Object equivalents — are transport details, not part of the model.

## Handlers and aliases are variadic, functions are not

This section is about why a handler accepts an argument list of any length while a function does not, and why that asymmetry is forced rather than chosen. The short answer is that a handler is the in-process interpretation of `execve`, whose interface is `argv[]` — a vector of arbitrary length — so **a handler is variadic by necessity, not by taste**. You cannot mock `git` with a fixed-arity function, because `git status`, `git commit -m …`, and `git log --oneline -n 5` all reach the one handler installed for the name; the handler must take whatever the caller typed. A function is the other thing entirely: lambda application over a declared parameter list, as in `{ |x y| … }`, its arity fixed at definition and part of its type. The asymmetry is therefore the exec/lambda boundary — the identifications of *System calls are algebraic effects* and *Values and commands* — made visible at the point where you actually invoke things; the arity tells you which world you are in.

Concretely, a per-name handler is a unary lambda `{ |args| … }` invoked with the command's argument list, and the catch-all `handler:` is a binary lambda `{ |name args| … }` invoked with the name and the arguments. The calling convention is fixed by the surface position — per-name versus catch-all — not inferred from the value's runtime shape, and arity is validated once at install time, so a bare block or a wrong-arity lambda is rejected rather than coerced. The reason it must be rejected, not inferred, is currying: a binary lambda applied to one argument does not fault but returns the inner closure, so a mis-shaped handler would silently hand the body a partial-application closure as the command's result — a category error surviving as a plausible value, exactly the bug class ral exists to make unconstructable.

An alias is just a top-level handler — the same shape, the same variadic convention, and the same dispatch after the binding namespace has declined the name. It differs only in where and how long it is installed: in the interactive command namespace rather than scoped by `within`, and persisting across turns rather than popped at block exit. Hence scripts never see aliases at all, so script behaviour cannot turn on the user's interactive configuration — a property worth the narrowing on its own.

## A spare surface: not POSIX, two sigils

This section is about the visible grammar, and about the single discipline that shapes it: **the surface refuses to overload one form with two meanings.** This is where the value/command distinction of *Values and commands* becomes syntax you can point at, and it is where the break from POSIX lives.

Start with what we give up. POSIX shell compatibility is not a matter of matching a few operators; it requires word-splitting, glob expansion on unquoted variables, `$IFS`, and context-dependent quoting — the very machinery whose interaction produces the classic escaping bugs. We eliminate exactly these. Compatibility is therefore not a goal we narrowly miss but one we decline on purpose, because keeping it would mean keeping the collapse that *Values and commands* exists to refuse.

In its place stand two sigils, one for each operation that matters. `$` retrieves data — it consults only the value namespace and never triggers command lookup — whereas `!` runs a stored command. The two compose: `!$b` is *dereference, then force*. The reader may ask why two are needed when one would read more tersely. The answer is that a single sigil covering both would make *retrieve* and *run* indistinguishable at a glance, and — since blocks are ordinary values passed as data — that ambiguity would propagate into every expression that traffics in commands. Two sigils keep the call site honest about which thing is happening.

Five further decisions are the same refusal seen from other angles.

First, *no context-dependent lexer rules*. The token classes are fixed once, single-pass, with no mode the parser can flip — none of the separate arithmetic, test, or glob lexers a POSIX shell carries, because we have no history forcing them. An `IDENT` (the variable, key, and deref class) terminates on `:` and on `=`, so a name ends naturally there; a `NAME` (the bare-word class) does not, so command arguments such as `-DFOO=bar` and `http://host:port` stay single tokens. And `:` becomes a token of its own only before whitespace, `]`, or end of input — which is why `host: val` splits but `localhost:5432` does not.

Second, *paths are built by interpolation, not quoting*. Outside quotes `$name` is a separate atom, so `$dir/file` is two arguments and a path is written `"$dir/file.txt"`. This inverts the bash convention. There the unquoted form is dangerous and quoting tames it; here the unquoted form is already safe — there is no splitting to suppress — and it is the quotes that perform the concatenation.

Third, *paths are strings*. There is no `Path` type. Once word-splitting is gone the historical reason a shell needed path-specific quoting goes with it, and a textual UTF-8 value suffices.

Fourth, *one expression language*. `$[...]` spans arithmetic, comparison, and logic under a single Pratt grammar, rather than bash's `(( ))`/`[[ ]]` split — a partition its history forced by way of separate lexers, and one we have no reason to inherit. The `&&` and `||` inside `$[...]` are the Boolean connectives on strict `Bool`, not tests of command success.

Fifth, *no command-level `||`*. `try { a } { |_| b }` already replaces `a || b` in command context, so a binary `||` on pipelines would only buy precedence rules against `?` and `|` for a case `try` handles — grammar paid for nothing.

The cost of all this is plain: a lifetime of POSIX habit transfers imperfectly, and scripts must be rewritten rather than ported. We accept it because the habit being broken is precisely the one that breeds the bugs — and because a surface with two sigils and no hidden modes is one a reader can hold in the head whole.

## Derivations, not pillars

A handful of decisions look like pillars but are really corollaries, and it is worth saying which follow from what, lest the design seem larger than it is.  The small-core rule — that control flow is library, and exactly five operators (`within`, `grant`, `try`, `guard`, `audit`) earn a grammar arm by the two-part criterion of a non-HM typing rule and runtime semantics no thunk-taking function can express — is not a co-equal commitment but a corollary of *Scoped effect handlers*: handler installation is the very thing that fails the criterion, and the other four join it for the same structural reason.  The three-layer filesystem surface — structured queries that return values, bytes I/O through codecs and redirects, and effects through bundled coreutils — is derived from the algebraic-effects identification together with the value/command split: a structured return earns a value-side primitive, whereas an effect is an open external name and earns none.  The row-types choice — scoped labels after Leijen, so that a spread shadows rather than removes — derives from the same value side, since records are values and their fields must unify without absence markers.  And the syscall bridge — `list-dir`, `file-info`, the `is-*` predicates, `resolve-path`, `glob`, in place of parsing the text of `stat`, `ls`, or `realpath` — is the value/command split applied to the kernel: a query is a value, so it crosses the bridge, while an effect stays a command.  None of these is an independent axiom; each follows.
