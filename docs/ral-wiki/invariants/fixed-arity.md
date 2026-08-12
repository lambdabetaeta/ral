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
as many arguments as the call site writes, and `echo` and `detach` take a fully
open argv (`ArgSig::Any`, `core/src/typecheck/builtins.rs`)
([[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]).

**Arity decides which mechanism a name is, and nothing declares it.** A
manifest entry's arity is read off its type rule — a signature's argv shape, a
scheme's curry-spine depth (`BuiltinEntry::fixed_arity`) — and that reading is
the whole classification: fixed arity makes the name a first-class *native*
value, an open argv makes it a *base frame* on the handler stack, whose
arguments arrive as an argv like any command's
([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]).
So `$name` exists exactly where a fixed arity does, by construction rather than
by agreement between a declaration and a check.

**That partition is the interception rule too.** A fixed-arity entry is a
native seeded into the base env scope, which resolution reaches before the
handler stack, so no installed handler intercepts it under its bare name —
only `^name`, which skips the env by definition, reaches one. An open-argv entry
*is* a frame at the bottom of that stack, so a user frame stacks above it and
every bare call arrives there first. Handleability is therefore nothing an
entry states about itself; it is what its arity has already said.

**A spread is the notation of an argv.** `...$xs` splices a list into an argv,
so it may be written only where an argv exists: a command, an external, an
open-argv builtin, or a handler or alias arm. A value takes its arguments by
application at a declared arity, so it has no argv and `...` has nothing to
spread into — a spread in a value's argument position is **T0056**
(`SpreadIntoApplication`). A list literal's spread (`[1, ...$xs, 2]`) and a
rest-pattern (`[first, ...rest]`) are a different construct, building and
taking apart a list rather than an argument sequence, and this rule leaves them
alone.

**The discipline survives at that boundary by packing, not by banning.**
`run_handler` (`core/src/runtime/command_call.rs`) hands a handler its arguments
as one list value — `[argv]` for a per-name entry, `[name, argv]` for a
catch-all — so a variadic operation is consumed by a fixed-arity lambda typed
`Fun(List α, B)` ([[internals/handler-dispatch|handler-dispatch]]). Variable
arity is a surface phenomenon; at the value level it is always a list.

Optionality is data too, never a hole in an argument list: an
[[invariants/optionality-via-variants|open variant]] (`` `some v `` /
`` `none ``), passed as an ordinary value. The caller always supplies every
argument; the *value* it supplies may carry presence or absence, and no
signature spells an argument the call site may leave out
([[decisions/260812_no-value-has-an-optional-argument|no-value-has-an-optional-argument]]).

This is a hard rule, not a stylistic preference. Do not introduce variadic
application, a splat parameter, a defaulted argument on a function or prelude
binding, a spread in a value's argument position, or a first-class `$name` for
an entry whose argv is open.
