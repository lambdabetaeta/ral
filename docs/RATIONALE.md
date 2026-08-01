# `ral` — design rationale

The [specification](SPEC.md) says what `ral` is. This document says why it has
that shape: which distinctions carry the design, what follows from them, and
what each choice costs. It is motivation, not contract; the specification is
normative where the two disagree.

**`ral` is a shell that refuses to confuse data with action.** A shell has two
jobs: compute with values, and cross into an operating system. `ral` keeps those
jobs separate, then gives them explicit ways to meet.

The argument has two foundations:

- *call-by-push-value* separates inert values from commands which do work;
- an open external call is an *algebraic effect operation*, interpreted by a
  handler or, by default, the operating system.

The rest follows in layers: surface and scope; pipelines and structured
crossings; handlers and grants; failure and the grammar which remains.

## Values and commands

**A value *is*; a command *does*.** A string, list, record, or block may be
named, passed, and inspected without running anything. A command may read,
write, return a value, or fail.

Conventional shells collapse these categories. A string can be data, a command
name, or source to be split and evaluated again. Captured output is therefore
re-lexed, word-split, and glob-expanded. `ral` refuses that round trip: once bytes
become a value, the value is never silently turned back into source.

Call-by-push-value supplies the formal account. A block `{ M }` *thunks* a
command into a value; `!` forces it. A parameterised block `{ |x| M }` is the
same abstraction awaiting an argument. Blocks therefore replace the several
mechanisms conventional shells keep apart:

- functions;
- aliases;
- `eval`;
- subshell bodies;
- handlers and callbacks.

A bound block in head position is forced implicitly, so `greet alice` remains
ordinary application. Everywhere else the crossing is visible.

Capturing byte output in a `let` decodes it as strict UTF-8, strips one trailing
newline, and binds a `String`. Invalid text fails with a hint to finish the
pipeline with `from-bytes`. The convenience is deliberate, but so is the
strictness: text is easy, binary data is never corrupted to pretend that it was
text.

The cost is explicit suspension and forcing. `ral` pays it because an invisible
crossing from data to execution is dearer.

## System calls are algebraic effects

The value/command split leaves a question: what is an external command?
**A call such as `git status` is the performance of an algebraic effect
operation.** The operation is the open name and its arguments; its
interpretation is supplied separately.

Head lookup first offers a name to the language's bindings — bindings are the
primitives' home, so this is one lookup, not two. If the bindings decline it,
the name belongs to the open command interface. A handler may interpret it
in-process; otherwise the operating system resolves and runs it. The program
says *which operation to perform* without baking in *how that operation is
performed*.

This claim is about the shell's external-process boundary, not every host call
made by the implementation. `list-dir` and `file-info` reach the kernel, but
they are closed, structured primitives: their meaning is fixed by `ral` and their
result is a value. `git` is open and interpreted. Openness, not mere contact
with the kernel, marks the effect boundary: a name is a value or it is handled,
and the handler stack's base is the host's interpretation of the command-shaped
names.

Several consequences follow:

- authority is permission over operations, so it is checked where an operation
  would be performed;
- a failure is an operation's exceptional outcome, not a `Bool`;
- an audit is a trace of operations and the scopes which framed them;
- a handler may replace an operation's interpretation;
- `grant` may narrow the set of permitted operations.

Handlers do not need first-class continuations for this domain. An external
operation happens once, returns once, and the ordinary continuation consumes
its result. `ral` therefore keeps the continuation implicit and resumes it once
in tail position.

## One form, one meaning

**The surface makes the value/command boundary visible rather than recovering
it from context.**

- `$name` retrieves a value. It never performs command lookup.
- `!$block` forces a stored command.
- A bound name in head position is application, the one convenient implicit
  force.

The same rule explains why `ral` breaks from POSIX. POSIX compatibility requires
word splitting, `$IFS`, glob expansion on unquoted variables, and several
context-sensitive quoting and lexing modes. Those mechanisms are not an
incidental syntax to emulate; they are the data/source collapse `ral` is built to
avoid.

`ral` consequently has:

- one lexer, with no arithmetic, test, or glob modes;
- no implicit word splitting or globbing;
- interpolation for construction rather than quoting for protection;
- strings for paths, because a path no longer needs a second quoting
  discipline;
- one `$[...]` expression language for arithmetic, comparison, and Boolean
  logic.

Outside quotes, `$dir/file` is two arguments. `"$dir/file"` is one constructed
string. Quoting concatenates; it does not disinfect.

The cost is real: POSIX habits transfer imperfectly, and scripts are rewritten
rather than ported. The gain is a surface small enough to hold in one head,
with no form whose meaning changes behind the reader's back.

## Lexical data, dynamic authority

**Data is lexically scoped; ambient authority is dynamically scoped.** The two
kinds of context answer different questions and should not share a mechanism.

A `let` binding is immutable. Re-`let` introduces a fresh binding which shadows
the old one, and a closure captures the bindings present where it was defined.
A name's meaning is therefore its defining expression, not the history of
assignments which happened to precede its use.

Stateful iteration threads state explicitly through `fold`, `reduce`, or
`fold-lines`. This gives up mutable accumulators, but buys local reasoning and
safe concurrency: a `spawn` receives an isolated copy of the captured
environment, with no shared cell on which parent and worker can race.

The working directory, environment overlays, handlers, and capability
restrictions follow the call instead. `within` and `grant` push dynamic frames
for the extent of a body, so a function written elsewhere still obeys the
directory, environment, interpretations, and authority of its caller.

The braces show where a context begins; dynamic reach ensures that a callee
cannot escape it merely because it was defined outside.

## Pipelines follow their edges

**A pipe is dataflow, and the type of its connecting edge chooses the execution
model.** No keyword and no runtime guess selects between the two cases.

- A *value pipeline* is data-last composition. When every stage and edge is on
  the value channel, `x | f` reduces to `f !{x}` and runs sequentially in the
  parent evaluator. No process or OS pipe exists.
- A *byte pipeline* is a process pipeline. If a stage is external or an edge
  carries bytes, every stage runs in a subprocess, the stages share one process
  group, and the parent shell remains outside that group.

The parent must remain outside a byte pipeline's process group so it can hand
the terminal to the group and later reclaim it. A shell cannot both manage a
foreground group and be one of its members.

Typed codecs make the crossing explicit. `from-X` consumes bytes and returns a
value; `to-X` consumes a value and emits bytes. A value cannot drift into an
external command merely because its printed form looks plausible.

Failure is independent of this routing. `|` moves data; it does not branch on
success. A failing stage fails the pipeline, while `?` and `try` decide how to
recover.

The cost is process isolation. A `ral` stage inside a byte pipeline is a subshell:
its working-directory, environment, alias, and module changes do not flow back
to the parent. Only the pipeline's data and final value cross the boundary.

## Structured values cross once

The operating-system boundary has three deliberately different routes:

- *queries* such as `list-dir`, `file-info`, and `glob` return structured
  values directly;
- *bytes* cross through redirects and the typed `from-X` / `to-X` codecs;
- *effects* retain command names such as `cp`, `mv`, `mkdir`, and `rm`.

A query belongs on the value side because parsing the textual output of `ls` or
`stat` would perform a bytes-to-text-to-record round trip for no benefit. An
effect stays a command because a second structured spelling would add no value
and create another path which capability enforcement must cover.

This also keeps danger legible. Recursive destruction is written `rm -rf`; no
pleasantly named `remove-file` primitive gets to decide, invisibly, whether
recursion is required.

Structured results need types which compose. `ral` uses Hindley–Milner inference
with open rows and scoped labels. A record spread prepends fields, duplicate
labels are retained, and lookup selects the first, so a spread shadows without
requiring absence predicates or a restriction operator. This is Leijen's
scoped-label discipline: a modest extension to unification rather than a second
record calculus.

The cost is that shadowing is one-way at the value level: there is no record
operation which removes the first field to reveal the one beneath it. Dynamic
context uses stack frames where later re-exposure is actually needed.

## Effect handlers reinterpret external names

**A handler supplies another interpretation for an open operation.**

`within [handlers: [git: H], handler: K] { body }` installs interpretations for
the dynamic extent of `body`. Resolution is env-first, so a lexical binding, a
native, or a prelude function retains its ordinary meaning at a bare head even
where a handler of the same name is installed; only a name which reaches the
open command interface, or an explicit `^name`, is handled.

Handlers in `ral` are deliberately restricted:

- *deep*: the handler remains installed for later operations in the body;
- *self-masking*: while the handler runs, its own frame is absent, so forwarding
  the same name reaches an outer handler or the operating system;
- *tail-resumptive*: the returned value replaces the operation and the body
  resumes once, without a first-class `resume`.

Self-masking makes wrap-and-forward finite:

```ral
within [handlers: [git: { |args| git ...$args }]] {
  git status
}
```

The calling convention comes from the boundary being interpreted. A per-name
handler is a unary function receiving the whole argv as one homogeneous list;
the catch-all is a binary function receiving the name and that list. Ordinary
functions still have the parameter list declared by their lambda. There is no
variadic exception in the value calculus: variable shell arity is packed into
one value.

Arity is checked when a handler is installed. Inferring it at dispatch would be
unsafe under currying: applying one argument to a binary lambda returns a
closure, so a malformed handler could otherwise succeed with a plausible but
nonsensical result.

An alias is the interactive, persistent form of the same idea: a top-level
per-name handler with the same one-list convention. Scripts do not see aliases,
so their behaviour cannot depend on a user's interactive configuration.

The cost is expressiveness `ral` does not need: handlers cannot capture, discard,
or invoke a continuation more than once. In return, their control flow matches
the once-only external operations they interpret.

## `grant` attenuates authority

**`grant` is authority over effects, narrowed by intersection.**

`grant [exec: ..., fs: ..., net: ...] { body }` places a visible bound around a
body whose dynamic extent includes every callee. Each capability axis composes
independently:

- an omitted axis inherits the ambient authority;
- a named axis can only narrow it;
- nested grants meet;
- a denial cannot be reopened by an inner grant.

The body still evaluates in the caller's process. `ral` checks operations it can
see at their in-process dispatch point, and confines spawned children with the
platform sandbox. These are complementary enforcers: the OS cannot see a
structured action `ral` performs itself, while `ral` cannot inspect every action a
child performs after `exec`.

Relative filesystem paths are resolved against the scoped working directory
before policy matching, with aliases such as symlinks accounted for. A
`within [dir: ...]` inside a grant therefore changes where a path starts, not
which paths the grant permits.

Network restriction has no in-process substitute because `ral` exposes no network
primitive. Where the platform cannot confine a child's network access, a
network-restricting grant fails closed.

The capability model is deliberately honest about its limits. A bare permitted
command name still depends on `PATH` integrity, path checks face the usual
time-of-check/time-of-use surface, and platform sandboxes differ. These are
named seams to harden, never reasons to pretend that an advisory check is a
sandbox.

This boundary is also used by `exarch`: an agent run executes beneath a
host-supplied grant on the same dynamic stack.

## Failure is not truth

**Failure is a runtime outcome; `Bool` is a value.** `ral` never asks a Boolean to
stand in for an exit status, or an exit status to stand in for a proposition.

- `if` branches on a `Bool`;
- `?` tries the next command only when the preceding command fails;
- `try` turns a failure into an error record and chooses recovery;
- `guard` runs cleanup and preserves the original failure;
- `audit` turns execution, including failure, into structured data.

A predicate returning `false` succeeds with the value `false`. Conversely, an
external tool whose exit status is data must be handled explicitly. This is why
`ral` has no command-level `||`: `try` already expresses recovery without adding
another precedence relation against `?` and `|`. The `||` inside `$[...]` is
only Boolean disjunction.

## The grammar is the residue

Most control flow is library: `for`, `while`, `map`, `each`, and `retry` are
ordinary functions over values and thunks. A construct earns a grammar arm only
when both of these are true:

1. ordinary Hindley–Milner typing cannot express its rule;
2. a function taking thunks cannot implement its runtime scope honestly.

Exactly five control operators qualify: `within`, `grant`, `try`, `guard`, and
`audit`. Each manipulates a dynamic frame, failure continuation, or audit
ownership which no ordinary value can observe. `if`, `case`, and `?` remain
syntax for their branching shape, but do not enlarge the primitive dynamic
machinery.

The small core is therefore not another foundation. It is what remains after
the value language, effect boundary, and scoping discipline have done their
work.

## Influences and relatives

- **rc** and **es**: lists and functions as values; control structures in a
  library.
- **Algol 60** and **Modernised Algol**: block structure, lexical scope, and the
  distinction between a value and a computation producing one.
- **Levy's call-by-push-value**: values, computations, thunks, and forcing.
- **Plotkin and Power; Plotkin and Pretnar**: algebraic operations and handlers.
- **Haskell** and **Backus**: immutable bindings, combinators, and equational
  reasoning in the pure fragment.
- **Tcl**: commands as ordinary names resolved at evaluation time.
- **Shill** and capability systems: explicit, attenuable authority.
- **Leijen**: extensible records with scoped labels.
- **JavaScript** and **Rust**: destructuring, closures, spread, and string
  interpolation.

The closest contemporary relatives are **YSH/Oil** and **nushell**. YSH/Oil
retains POSIX compatibility; nushell requires structured data throughout a
pipeline. `ral` chooses neither. It keeps Unix byte pipelines where bytes are the
right interface, values where values are the right interface, and a typed,
explicit crossing between them.
