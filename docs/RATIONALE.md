# ral — design rationale

This document is about why ral is the way it is.  The specification
says what ral is.  Here we record, for each design choice, the
alternatives it beat and the reasoning behind it — motivation, not
contract.  §20 of the spec fixes the underlying calculus.

The document is organised into seven parts.  **Foundations** states the
calculus and the one identification — external commands are algebraic
effects — from which the dynamic design follows.  **Surface decisions**
covers the visible grammar and binding rules.  **Control and effects**
explains what earns a grammar arm, and how scoped contexts, handlers,
and capabilities compose.  **The filesystem surface** explains the
asymmetry between structured queries and effects.  **Types** records the
row-types choice.  **Runtime** covers the two pipeline models and
concurrency.  **The interactive layer** covers aliases, modules, and
plugins.  Implementation mechanics are out of scope here; the spec is
normative where the two disagree.

# Foundations

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

## Values and commands

The organising distinction is between *values* — inert data, named,
passed, inspected — and *commands* — effectful processes that may read,
emit bytes, return a value, or fail.  Most shells collapse the two.
Every datum is a string, and every string is simultaneously data, a
command name, and source text for further evaluation.  The consequence
is that captured output is re-lexed, split on whitespace, and
glob-expanded.  We refuse this collapse, and thereby avoid the class of
bugs that arises from it, without sacrificing first-class commands and
pipes.

The formal account is call-by-push-value (Levy).  Values are inert;
computations are effectful; a thunk packages a computation as a value;
and forcing runs it.  One need not know the theory to use the shell.
The slogan is that a value *is* and a computation *does*, and at the
surface `{M}` thunks while `!` forces.

## System calls are algebraic effects

The value/command distinction leaves one question open: what *is* a
command? **Our answer is that running an external command is the
performance of an *algebraic effect operation*.** A name such as
`git`, which the binding namespace declines, is an *operation*.  It is
performed, it returns once with a result or a failure, and its meaning
is supplied separately by its *interpretation* — by default, the
operating system carrying out the syscall.  This separation of the
operation from its interpretation is the defining move, and the rest
of ral's dynamic design rests on it.

Not every kernel call is an effect in this sense.  A structured-query
builtin such as `list-dir` or `file-info` reaches the kernel yet is
kept inside the pure value language, answering with records rather
than bytes, precisely so that the capture-then-reparse round-trip
never arises.  What makes a name an operation is being an open external
name whose meaning comes from its interpretation, not merely the fact
of touching the OS.  We therefore draw the effect boundary at the
external-process surface — the `exec`/`wait` family that defines a
shell.

Two further choices are *orthogonal* to this principle.  First, whether
a program may *reinterpret* an operation rather than defer to the OS.
Second, should it reinterpret, whether the reinterpreting handler may
capture the continuation.  Both belong to *Effect handlers* below, and
on each we take the minimal setting.  The principle stands without
either: an external command is an effect whether or not any handler
intercepts it, and performing one needs no captured continuation — the
syscall is a single forward step, and the continuation is the ordinary
rest of the computation, never a reified value.  From the
identification alone follow scoped authority (a capability is
permission over the effect set), the capability check at the point an
effect is performed, audit as a record of the operations performed,
and failure as an operation's exceptional outcome.

## Blocks as the single abstraction mechanism

`{M}` stores a command; `{ |x| M }` is a function; `!` runs either.
This single mechanism replaces the several of conventional shells —
functions, aliases, `eval`, subshells, trap handlers.

Forcing is always explicit, with a single exception: a bound name
resolved in head position is forced (or applied) implicitly.  This
keeps ordinary calls natural (`greet alice`) while keeping the storage
of commands visible (`let plan = { make build }`).

## Two sigils

`$` retrieves data; `!` runs stored commands.  `!$b` composes:
dereference, then force.  A single sigil covering both would make
'retrieve' and 'run' indistinguishable at a glance, and the ambiguity
would then propagate into every expression that passes blocks as data.

# Surface decisions

## Shadowing, not mutation

All `let` bindings are immutable; re-`let` shadows within the enclosing
scope.  Closures capture at definition time, so equational reasoning
holds in the pure fragment.  The cost is the absence of mutable
accumulators; `fold`, `reduce`, and the streaming `fold-lines` replace
them.  The benefit is twofold.  First, local reasoning is easier.
Second, `spawn` is safe without synchronisation — the isolated copy
shares nothing that can be mutated concurrently.

## External commands return strings

When an external command's stdout is captured in a `let`, the runtime
decodes it as UTF-8, strips one trailing newline, and binds the result
as `String`.  Invalid UTF-8 fails with a message naming the command
and suggesting `| from-bytes`; `Bytes` remains available via that
terminator.

This trades a sliver of generality for a large reduction in ceremony.
Most command output is text.  Demanding an explicit decode on every
binding generates noise without adding protection beyond what a strict
error already gives.  The returned `String` is data — never re-lexed,
split, or globbed — so the classic capture-then-reparse chain does not
arise.

## Piping and failure

`|` and `?` are deliberately separate: `|` moves data between stages,
whereas `?` reacts to command failure.  Exit status and data flow thus
remain distinct concerns.  `if` branches on `Bool`, not on command
success; a predicate returning `false` still *succeeds*, so confusing
'false' with 'failed' is impossible.  When success must be inspected as
data, `try` is the mechanism.  When a command dies from a signal or is
stopped by terminal job control, we keep that distinction in the error
we report, rather than collapsing everything to a numeric status too
early.

## No command-level `||`

`try { a } { |_| b }` replaces `a || b` in command context.  A binary
`||` on pipelines would force precedence rules relative to `?` and
`|`, adding grammar for a case `try` already handles.  The `||`
operator that *does* exist is the Boolean connective inside expression
blocks, `$[a || b]` (§2, SPEC).  It operates on `Bool` values, not on
command success.

## Expression blocks

`$[...]` is one expression language, spanning arithmetic
(`+ - * / %`), comparison (`== != < > <= >=`), and logic
(`&& || not`).  Unlike bash, which partitions these into `(( ))`
and `[[ ]]` because its history forced separate lexers, we have no
such history.  Comparisons already cross the numeric/Boolean boundary
by returning `Bool`, so the simplest and most honest grammar is
one.  `&&` and `||` short-circuit; operands are strict `Bool` —
`$[1 && true]` is a type error, not truthy.  The `not` keyword
carries unary negation, because `!` is already force (`!{...}`)
and `~` is tilde expansion; context-dependent symbol overloading
would be the worse trade.

## Data-last argument order

`map f items`, `fold f init items`, `filter p items`.  Piping and
partial application then align: `items | map $f | filter $p` reads
left-to-right, and `map $f` is itself a function waiting for its list.

## No context-dependent lexer rules

The token classes are fixed once, single-pass, with no mode the parser
can flip.  `IDENT` (the variable/key/deref class, §1.1 SPEC) terminates
on `:` and on `=`, so names end naturally at these characters; a `NAME`
(the bare-word class) does not, so that command arguments such as
`-DFOO=bar` and `http://host:port` stay single tokens.  `:` becomes a
token of its own only before whitespace, `]`, or end of input, which
is why `host: val` splits but `localhost:5432` does not.

## Path construction uses interpolation

Outside quotes, `$name` is a separate atom — `$dir/file` is two
arguments.  Paths are built by interpolation: `"$dir/file.txt"`.  This
inverts the bash convention, where quoting suppresses word-splitting.
Here the unquoted form is already safe — there is no splitting — and it
is quoting that performs concatenation.

## Paths are strings

There is no `Path` type.  Textual values are UTF-8, and the absence of
word splitting removes the historical reason shells needed
path-specific quoting.

## `let` unifies binding, capture, and storage

The `let` RHS is a command context, and a single mechanism covers
three operations:

- `let x = foo`        runs `foo` and binds its result;
- `let x = 'foo'`      binds the string;
- `let f = { |x| … }`  stores the block.

Bare words run commands; quoted words are data; and value forms
(literals, blocks, lists, maps, derefs, arithmetic) receive an
implicit `return` in command context.  We thus preserve the shell
convention — unquoted words run commands — without collapsing the
language into strings.

## Not POSIX

POSIX shell compatibility requires word-splitting, glob expansion on
unquoted variables, `$IFS`, and context-dependent quoting.  We
eliminate exactly these.  Compatibility is therefore a non-goal.

## Termination: `return`, `fail`, `exit`

Scripts end at the last statement.  Three primitives end them earlier,
each with its own scope:

- `return` exits the current block or file with success.  Inside a
  sourced file, it stops *that* file, not the caller — so that a
  `return` in a library never kills the script that loaded it.
- `fail` aborts the current evaluation with nonzero status and an
  error record:

      fail [status: 1]
      fail [status: 7, message: 'config missing']
      fail $e                          # re-raise inside a try handler

  Errors are values, not numbers.  The record produced by `try { ...
  } { |e| ... }` is exactly the input shape `fail` accepts, so
  wrap-and-rethrow composes without dropping fields.
- `exit N` (alias `quit`) terminates the whole shell process with
  status `N`.  It is reserved for top-level use; scripts that want to
  halt cleanly should prefer `return`.

# Control and effects

## Control flow is library; five control operators are syntax

The grammar knows about exactly five control operators — `within`,
`grant`, `try`, `guard`, `audit` — plus the two purely-syntactic
forms `if` and `case` and the chain operator `?`.  Everything else
that looks like control flow (`for`, `map`, `each`, the prelude's
`attempt` and `retry`) is an ordinary parameterised block in the
prelude.  The split is principled, not pragmatic.

A construct earns a grammar arm just if *both* of the following hold:
its typing rule is not derivable from ordinary Hindley–Milner over the
builtin signature, and its runtime semantics cannot be expressed as a
function taking thunks without lying about that signature.  The five
operators meet both conditions:

- `try B H` has a custom typing rule that unifies `B`'s output with
  `H`'s output and threads the error record into `H`'s parameter; no
  monomorphic builtin signature captures this.
- `guard B C` mediates which failure escapes (the body's; never the
  cleanup's) and which is logged-and-discarded.
- `within OPTS B` and `grant CAPS B` manipulate dynamic frames
  (working directory, environment, effect handlers, attenuable
  capabilities) that live in `Shell` state, not in any value the
  body can observe through its parameters.
- `audit B` *owns* the audit subtree its body produces; the
  tree-shape question — which scope's children does this node belong
  to — is structural and cannot be answered after the fact by a
  function that received `B` as a thunk.

Surfacing these as keywords also shrinks the surface elsewhere.  The
five names are reserved in `let`-binding position and in bare-head
command position; `^try` keeps PATH-lookup semantics; `$try` and the
other four in value position are compile-time errors with a targeted
diagnostic.  The IR carries a `Within`/`Grant`/`Try`/`Guard`/`Audit`
node per operator with named structural fields, and a `Redirect`
wrapper for the trailing-redirect case — none of the five appear as
string-keyed builtins anywhere in the typechecker or evaluator.

`if`, `case`, and `?` remain in the grammar for a different reason:
they take an arbitrary number of arms and need parser support to keep
the surface readable, even though their typing is ordinary.  Everything
else stays in the prelude.  The parser does not grow when a user
defines `retry`; it grows only when a new wrapper needs handler-frame
manipulation, audit-tree ownership, or a typing rule outside HM.

## Scoped execution contexts

`within` and `grant` are properties of the execution context, not of
source text.  A function defined in one module and called inside a
restricted block runs under that restriction.  `within [env: [KEY:
VAL]] { body }` overrides environment variables; `within [dir: PATH] {
body }` overrides the working directory.  Both are facets of a single
scoping primitive.  The idea is simple: lexical capture is the right
model for data, whereas dynamic inheritance is the right model for
ambient authority.

`grant` is a *capability wrapper around a block*: it narrows the active
authority for the duration of `body` and otherwise composes like any
other block-bodied builtin.  The block itself always evaluates locally,
in the caller's process; what an `fs`/`net`-restricting grant adds on a
platform with OS sandboxing is per-command child confinement — each
external or bundled command spawned inside the block is launched under
the effective platform sandbox, while ral's own dispatched effects are
gated in-process by the capability checks.  The caller observes only the
outcome and the captured audit/byte observations.

## Effect handlers: deep with self-masking

Handlers are the orthogonal reinterpretation layer of *System calls
are algebraic effects* above.  They let a program supply its own
interpretation of an operation in place of the OS default.  They are
additive — the effect identity holds whether or not any handler
intercepts a command — and ours are a deliberately small fragment:
tail-resumptive with no first-class `resume`, and so less than
algebraic-effect handlers usually offer.

`within [handlers: …, handler: …]` installs effect handlers on a
dynamic stack.  Each per-name handler (and every alias) is a unary
lambda `{ |args| … }` invoked with the command's argument list, and
the catch-all `handler:` is a binary lambda `{ |name args| … }`
invoked with the command's name and arguments; the calling convention
is fixed by the surface form, so a bare block or a wrong-arity lambda
is rejected at install time rather than coerced.  Two independent
design questions then arise, often conflated under one 'deep vs
shallow' heading; we commit to a definite answer to each.

Handlers interpret open operation names after the language binding
namespace has declined the name.  Lexical bindings, prelude names, and
builtins are not shell aliases: a handler cannot replace `length`, and
a local `let foo = ...` beats an active handler for `foo`.  This keeps
ordinary language names stable while preserving command mocking for
open external names such as `cat` or `git`.

**Deep vs shallow** is the question of whether a handler `H` persists
across the continuation of the operation it handles.  After
`within [handlers: [git: H]] { git a; git b }`, both `git a` and `git b`
trigger `H`; the installation is not consumed by the first call.  By
the standard criterion (Plotkin–Pretnar deep handlers re-wrap `H`
around the continuation; Hillerström–Lindley shallow handlers do not),
ral's handlers are **deep**.

**Self-masking vs self-transparent** is a separate question: during
the evaluation of `H`'s body, is `H` itself still in scope? In ral,
the matched frame is lifted off the dynamic stack for the dynamic
extent of the handler body — so a call to the same name from inside
`H` reaches the next outer frame, or the OS, never `H` itself.  ral's
handlers are **self-masking**.

Without `resume`, deep handlers can re-trigger themselves only through
a raw recursive call from inside the handler body.  The Plotkin–Pretnar
calculus avoids this issue because all re-entry happens through
`resume k`, and `k` evaluates under `H` by construction; the
continuation discipline does the work.  ral has no `resume` —
handlers receive the command name and arguments and return a value or
fail.  Self-masking is therefore the operational rule that keeps the
dominant idiom (`within [handlers: [git: { |args| my-git ...$args }]]`) free of
infinite recursion without requiring `^git` inside every handler
body.

The shell intuition is the same as a POSIX function named `git` whose
body wants to call the real `git`: the function shadows the name only
*outside* itself.  ral generalises this to the whole handler stack,
typed and lexically scoped, with `^name` available as an explicit
bypass of the lexical/prelude/builtin binding chain (handlers still
apply, because `^name` is a syntactic flag on the lookup, not a
frame-unwind).

This combination is the practical content of ral's effect-handler
design: deep, so `within` covers its dynamic extent the way the user
expects; self-masking, so the wrap-and-forward idiom is the natural
reading and not a recursion trap.

## `guard`, not `on EXIT`

`guard` wraps a body, runs cleanup regardless of outcome, and
propagates the original failure unchanged.  It is scoped and lexically
apparent.  Registration-based cleanup (`on EXIT`), by contrast, is
mutable global state whose ordering follows execution flow rather than
source structure, and composes poorly with nested error handling.

## `try` and `audit` are separate operators

`try` traps failure and dispatches to a handler that receives a small
structural record (`status`, `cmd`, `message`, `line`, `col`); it is
otherwise transparent, in that it does not redirect fd 1 or fd 2, so
side-effects inside the body remain observable as they happen.
`audit` builds the full execution tree, recording per-command bytes
regardless of outcome.  Separating them keeps the common case
(catch-and-handle) from paying for the uncommon one (full tracing),
and lets the two compose: `audit { try { … } { … } }` traps errors
*and* records bytes.  Both are control-operator keywords (§ "Control
flow is library; five control operators are syntax"), so the typing
rules are dedicated rather than shoehorned through a builtin scheme.

## Audit is one mechanism

Every audit-producing site goes through the same lexical scope: the
scope-introducing operator (`grant`, `within`, `guard`, `try`,
`audit`) owns the nodes its body produces.  Process boundaries —
each external or bundled command confined under an `fs`/`net`-restricting
grant, each pipeline stage helper — only transport audit fragments;
they never decide tree shape.  The wrapping scope merges incoming
fragments into its own child trail, so reports stay readable: a
sandboxed `grant { … }` shows its body's nodes as direct children
of the `grant` node, not loose at the root.

# The filesystem surface

## Three layers, one asymmetry

The filesystem surface is split into three layers:

1. **Structured queries** — primitives that return values: `list-dir`,
   `file-info`, `file-empty`, `line-count`, `temp-dir`, `temp-file`,
   `resolve-path`, `glob`, and the `is-*` predicates.  These drive a
   structured pipeline; they have no shell-tool analogue worth
   bothering with.
2. **Bytes I/O** — codecs (`from-string`, `to-json`, …) plus
   redirects: `to-json $v > $path` writes, `from-string < $path`
   reads.  Atomic-rename-on-write is built into `>` for regular files.
3. **Filesystem effects** — bundled coreutils (`cp`, `mv`, `rm`,
   `mkdir`, `ln -s`, `chmod`, …).  Effects don't return structured
   values, so a ral-native primitive would buy nothing the shell form
   doesn't already give; there are none.

The asymmetry is the design: structured returns earn a primitive;
effects do not.

Core keeps the universal part of that surface.  Exarch adds its
agent-facing search atoms (`grep-files`, `line-hash`, `explore-dir`)
and source-level edit helper as host extensions, because they are a
model workflow rather than a shell language requirement.

## The dangerous verb wears its name

Destruction is never abstracted.  Removing a directory tree is written
`rm -rf`: the flag that recurses is visible at the call site, written
on purpose by a caller who can see what they typed.  A polite wrapper
that decides for itself whether to recurse hides exactly the decision
a shell must keep visible.  Effects therefore go through the bundled
coreutils, where the destructive flag is part of the name.

## Bundled coreutils are mandatory in exarch, optional in ral

A sealed exarch profile that depends on host coreutils is not sealed —
it is reproducible only modulo whatever `cp` or `mv` the host happens
to ship (BSD vs GNU drift, version skew, locale defaults).  Exarch
therefore bundles a curated coreutils set and pins behaviour.  The
binary-size cost is paid once per profile build and is the price of
"I know exactly what's in this".

The bare `ral` binary keeps coreutils behind a feature flag.  An
interactive shell on a developer machine has system coreutils
already; there is no reason to ship 30+MB of duplicate tools.

## Capability-checked dispatch for bundled tools

Every uutils invocation goes through a wrapper that consults the
tool's own clap parser to find the path-argv positions, then calls
the same `check_fs_read` / `check_fs_write` that the structured
primitives use.  Bypassing the sandbox by reaching for `cp` instead
of a primitive is therefore not possible — both paths land at the
same chokepoint.  `within [dir: ...]` scope propagates by chdir under
a per-call lock, so relative paths resolve against ral's scoped CWD,
not the host process CWD.

## Syscall bridge, not text parsing

The structured query builtins — `list-dir`, `file-info`, the `is-*`
predicates (`is-file`, `is-dir`, `exists`, …), `resolve-path`, and
`glob` — replace shelling out to `stat`, `ls`, or `realpath` and
parsing their text.  Platform differences and the perpetual
bytes–text–structured round-trip disappear.  Effects are not in the
bridge — they are bundled commands invoked through the
capability-checked dispatch.

# Types

## Record types and scoped labels

The checker infers per-field types for map literals with static keys.
The representation is a *row*: a list of `(label, type)` pairs with an
optional tail variable standing for unknown fields.  Field access
unifies the target with `[label:α | ρ]` and returns `α`.  The unifier is
that of [Rémy 1989]: mismatched head labels permute past each other
into a shared fresh tail.

The spread `[...$base, port: 9090]` raises the question of duplicate
labels: if `$base` already has `port`, the result has two.  Rémy's
original system assumes uniqueness, and would require absence markers
(`Pre(T)` / `Abs`) and a restriction operator `ρ ∖ port`.  Introducing
them means new row constructors and changes to unifier, generaliser,
and display.

ral instead adopts the scoped-label row types of [Leijen 2005].
Duplicates are permitted in rows; selection always takes the first;
extension prepends, shadowing the prior entry rather than removing it.
The key observation is that the Rémy rewrite rule already treats
duplicates correctly — it swaps only *different* labels past each
other, so same-label entries keep their relative order.  No changes to
unifier, generaliser, or occurs check are required.

Effect: `[...$base, port: 9090]` with `$base : [host: String | ρ]` infers
as `[port: Int, host: String | ρ]`.  The explicit field prepends over the
spread's row variable, which becomes the result's open tail.  Shadowed
duplicates are invisible to selection and suppressed in display.  With
multiple spreads the result is open but imprecise — chaining two
arbitrary rows needs row concatenation, which is not part of Leijen's
system and is not included.

# Runtime

## Byte pipelines are processes; value pipelines are folds

Pipelines have two distinct execution models.  The type of the adjacent
edges decides which one runs.

A pipeline whose every stage operates on values is just typed
data-last composition: `x | f` reduces to `f !{x}`, evaluated
sequentially in the parent.  No process is spawned, and no pipe exists.
This is the path `range 1 21 | filter $even | sum` takes — three
function calls threaded by the value channel.

A pipeline that touches bytes — at least one external command, or any
byte edge — runs as a Unix-style process pipeline.  Every stage,
including ral-implemented ones, executes in a subprocess; all
subprocesses share one process group; the parent ral process is *not*
a member of that group.  This shape is what makes `cat README.md |
glow -p` work: the kernel sees one foreground process group containing
every stage that can touch the terminal, regardless of whether `cat`
is `/bin/cat`, an alias, a handler, or a ral block that wraps `bat`.

Keeping the parent out of the process group is what lets job control
remain coherent: a shell process that participates in its own
foreground pipeline cannot consistently both own the terminal and not
own it.  The price is that a ral stage running out-of-process is a
*subshell with respect to mutation* — a helper stage's `cd`, `env-set`,
alias / module / registry updates, or REPL changes do not flow back to
the parent; only the pipe contents and the final value cross the
boundary.  This matches the way every traditional shell treats process
pipeline stages.

The platform mechanics that realise this shape — the Unix process
group and `tcsetpgrp` handoff, the exec trampoline that wins a
foreground-claim race, the helper-frame protocol, and the Windows Job
Object equivalents — are transport details, not part of the model.

## Concurrency: isolation, not shared state

`spawn` creates an isolated copy of the evaluator.  There is no shared
mutable state and no synchronisation.  `await` is the only channel.
A second `await` on the same handle returns the cached result,
avoiding the need for affine types or runtime traps on aliased
handles.

A spawned handle buffers its output and replays it on `await`; a
watched handle (`watch "label" P`) streams each line live to the
caller's stdout, prefixed `[label] ` (stdout) or `[label:err] `
(stderr).  `watch` is an ordinary builtin (arity 2), not a keyword.
The framing lives in a single `Sink` variant — `LineFramed` — that
buffers bytes until `\n` and emits `prefix + line + '\n'` as one write
to the caller's stdout; sibling watchers serialise through the OS
stdout lock (or, under the interactive REPL, rustyline's external
printer) so each line is atomic even when several watchers run
concurrently.  Live watching hides the usual `cmd > /tmp/log &; tail -f`
scaffolding behind a library function.  We deliberately do not ship a
read-API on handles (`read-line $h`, `select-line [h₁,h₂]`): value
builtins like `each` are value-complete, so a handle-as-pipe-source
would require a streaming-internals refactor, whereas line-framed
watching satisfies the observed motivating use case at a much smaller
surface.

## Job control is narrower than bash, on purpose

Ctrl-Z parks the foreground command's process group as a numbered
job; `fg [N]` resumes it; `bg [N]` resumes it in the background;
`jobs` lists what is parked.  Beyond that, ral does not reproduce
the bash machinery that exists to compensate for the shell having no
persistent UI for parked work.

There is no exit-time refusal ("There are stopped jobs"), no
asynchronous `[N] Done <cmd>` print stream interleaved with the next
prompt, and no `%1` / `%+` / `%-` short-form addressing.  Job state
is observed by typing `jobs`, not by being interrupted at the
prompt.  The bash exit-time guardrail and the async notification
stream both exist because the shell has no other UI for jobs.  The
trade — printf-into-readline that occasionally collides with what
the user is typing — is the cost of that compensation.  Modern
multiplexers (tmux, zellij) cover the "I want a second running
thing" use case better than juggling jobs in one shell, so ral's
narrower surface is sufficient for the case Ctrl-Z genuinely needs
to handle: vim's drop-to-shell idiom and the same pattern in
`less`, `man`, and `top`.

The kernel mechanism — SIGTSTP delivery to the foreground process
group, `tcsetpgrp` for terminal handoff, `waitpid(..., WUNTRACED)`,
SIGCONT to resume — is unchanged from the canonical Unix
implementation.  The narrowing is in the prompt-side bookkeeping,
not the OS-side machinery.

Pure-value pipelines are sequential folds in the parent evaluator.  No
process exists for SIGTSTP to suspend, so they are outside this
discussion entirely.

# The interactive layer

## Aliases are semantic, not syntactic

Aliases live in the interactive command namespace, resolved at
evaluation time after value-head lookup, active only in interactive
mode.  Scripts never see them, so script behaviour cannot depend on
the user's interactive configuration.

## `source` is kept for configuration

`~/.ralrc` and interactive configuration need scope merging, which
`use` (returning a module map) does not do.  `source` exists for this.
`use` remains the default for library code.

## Plugins are modules, and run with host authority

A plugin is an ordinary ral module (§8) whose return value is either
a manifest map or a block that takes an options map and returns a
manifest map.  There is no plugin DSL, no separate loader language,
and no magic `$config` binding: a plugin's knobs are fields on the
options map it receives.  `_plugin 'load' 'fzf-files' [key: 'ctrl-t']`
evaluates the file, applies the options map to the returned block,
and reads `name`, `hooks`, `keybindings`, and `aliases` off the
resulting record.  Record destructuring, row polymorphism, and
`grant` already exist for other reasons; the plugin system is a
thin composition of them.

```
return { |options|
    let key = get $options key 'ctrl-t'
    return [
        name: 'fzf-files',
        capabilities: [
            exec: [fzf: []],
            fs: [read: ['.']],
            editor: [read: true, write: true, tui: true],
        ],
        keybindings: [[key: $key, handler: $_handler]],
    ]
}
```

Hooks (`buffer-change`, `pre-exec`, `post-exec`, `chpwd`, `prompt`)
fire on the live evaluator, not a clone.  They need to observe and
sometimes alter shell state — the `prompt` hook returns the prompt
segment, `chpwd` may update state cells.  The shell does **not** push a
capability frame around handler, keybinding, or plugin-alias dispatch:
plugins run with whatever capabilities the caller's stack already
grants.  A manifest may carry a `capabilities:` key, but it is advisory
documentation only — parsed and ignored at load.

This is a deliberate trade.  Plugin manifests are self-declared, so
in-process attenuation gives zero defence against an adversarial
plugin author and catches only honest-but-buggy plugins.  Paying the
conceptual cost — `with_capabilities` carrying two distinct meanings
(user-syntactic `grant` vs in-process plugin attenuation), bleeding
into the eval-boundary's transport dispatch — was disproportionate.
Users who want to confine a plugin call should wrap the call site in
`grant { … }`.  That is what `grant` is for, and it composes with
plugin code the same way it composes with any other code.

Handlers for an event run in load order; a failing handler's error
is logged but does not cancel siblings.  `buffer-change` runs under
a soft deadline (default 16ms) so a slow plugin cannot make typing
feel laggy; stale handlers are re-run at the next input idle.

## `_ed-tui` captures stdout

Interactive plugins invoke fuzzy finders (`fzf`, `sk`, …) that draw
on the terminal via `/dev/tty` and print the user's selection on
stdout.  A plugin needs that selection as a value.  If the body's
stdout went to the terminal, the selection would appear above the
prompt and the handler would get nothing back.

`_ed-tui` therefore opts into byte capture for the duration of
its body, analogous to what `let x = !{ … }` does at a binding site.
A non-`Unit` value from the body wins outright; otherwise the captured
bytes (trimmed of one trailing newline) are returned in the `output`
field of the result record `[output: Str, status: Int]`.  This is the
same "last command's bytes are the value" rule as `let`, applied
inside a higher-order builtin; body failures are caught internally and
reported via `status` so plugins switch on it without a `try`.

```
let dir = _ed-tui { fzf --walker dir +m }
```

## Keybinding dispatch is handler composition

Multiple plugins can bind the same key.  Dispatch walks handlers in
reverse load order: a handler returning `true` consumes the
keystroke, a handler returning `false` falls through to the next,
and if every plugin handler declines the shell runs its built-in
binding for the key.

This is the same shape as the `?` fallback chain for commands — a
stack of alternatives where each one decides whether to handle or
pass.  Load order controls precedence, the same way `use` order does
for bindings, so the user's last-loaded plugin wins by default and
earlier plugins remain reachable.

```
# if autosuggest's CTRL-F doesn't apply, the built-in binding still runs
load-plugin 'autosuggest' [:]
```
