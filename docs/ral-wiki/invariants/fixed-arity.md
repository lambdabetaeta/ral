# Fixed arity

**Fixed arity binds *application*: every invocable with a first-class `$name`
form — builtin, prelude function, user binding — takes a fixed number of value
arguments.** There is no variadic application and no defaulted parameter.

Arity is part of a computation's type. Under [[design/cbpv|call-by-push-value]]
a function consumes a fixed sequence of value arguments; a variable count would
make application irregular, and would leave a reified primitive's type
undefined ([[design/builtins|builtins]]).

**An entry that takes an argv is not applied, so the rule does not reach it.**
An external command, an intercepted operation, and a command-shaped builtin
take an *argv*, not a curried argument sequence: an intercepted operation takes
as many arguments as the call site writes, and `echo` and `detach` are variadic
over a list of strings, typed `List String -> Return(Bytes, Unit)` and
`List String -> F Any`
([[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]).

**The manifest is authored as two, and arity is the consequence.** A native
table entry declares its arguments, so it has an arity — the curry-spine depth
of its scheme, which is the one place any arity is read from
(`BuiltinEntry::fixed_arity`, a `usize` for every entry there) — and it is a
first-class *native* value. A base-frame manifest row takes an argv instead, so
the argv half has no arity at all: the row is a *base frame* on the handler
stack, whose arguments arrive as an argv like any command's
([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]],
[[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]). So
`$name` exists exactly where an arity does, by construction rather than by
agreement between a declaration and a check.

**Arity binds application; it does not bind writing a name.** A builtin is a
function and functions curry, so supplying fewer arguments than the arity is
not an error — it is the residual function, and `let f = length`,
`let f = to-json` and `let f = range 1` all bind one. A bare name and `$name`
agree about this; they differ only in that the first is a command and the
second a value.

**What is refused is a *discarded* value that is still waiting for an
argument.** A computation whose value is thrown away must be ready to run, not
a function: nothing will ever supply the rest of it, so it cannot have run, and
the statement did nothing. A value is discarded in exactly two places — a
non-tail part of a sequence, and the program itself, whose only surviving trace
is its status. A sequence's *tail* is not discarded: it is the block's value,
and `{ |x| $x }` is a block whose value is a function.

That is not a rule about builtins, and not a new one. It is the demand a
[[design/pipelines|pipeline]] stage already met — a stage must be a computation
ready to run, not an arrow still waiting — generalised from a stage to any
discarded value. Both readings name the verb when they can: bare `cd` is
`cd` expected 1 argument, got 0, rather than an anonymous shape mismatch.

So the two rules are about different things, which is why they do not conflict:
arity binds application, and readiness binds discarding. Over-application is
the arity rule's business (`upper "a" "b"` is one **T0050**, the surplus
inferred but unified against nothing, so a single slip earns a single
diagnostic); under-application is nobody's until the value is dropped.

**That split is the interception rule too.** A table entry is a native seeded
into the base env scope, which resolution reaches before the handler stack, so
no installed handler intercepts it under its bare name — only `^name`, which
skips the env by definition, reaches one. A base-frame row *is* a frame at the
bottom of that stack, so a user frame stacks above it and every bare call
arrives there first. Handleability is therefore nothing an entry states about
itself; it is which half of the manifest holds it.

**A spread is the notation of an argv.** `...$xs` splices a list into an argv,
so it may be written only where an argv exists: a command, an external, a base
frame, or a handler or alias arm. A value takes its arguments by application at
a declared arity, so it has no argv and `...` has nothing to spread into — a
spread in a value's argument position is **T0056**
(`SpreadIntoApplication`). A list literal's spread (`[1, ...$xs, 2]`) and a
rest-pattern (`[first, ...rest]`) are a different construct, building and
taking apart a list rather than an argument sequence, and this rule leaves them
alone.

**The discipline survives at that boundary by packing, not by banning.**
`run_handler` (`core/src/runtime/command_call.rs`) hands a handler its arguments
as one list value — `[argv]` for a per-name entry, `[name, argv]` for a
catch-all — every element rendered as it is packed, so a variadic operation is
consumed by a fixed-arity lambda typed `Fun(List String, B)`
([[internals/handler-dispatch|handler-dispatch]],
[[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).
Variable arity is a surface phenomenon; at the value level it is always a list
of strings.

Optionality is data too, never a hole in an argument list: an
[[invariants/optionality-via-variants|open variant]] (`` `some v `` /
`` `none ``), passed as an ordinary value. The caller always supplies every
argument; the *value* it supplies may carry presence or absence, and no
signature spells an argument the call site may leave out
([[decisions/260812_no-value-has-an-optional-argument|no-value-has-an-optional-argument]]).

This is a hard rule, not a stylistic preference. Do not introduce variadic
application, a splat parameter, a defaulted argument on a function or prelude
binding, a spread in a value's argument position, or a first-class `$name` for
an entry that takes an argv.
