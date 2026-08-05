# Fixed arity

**Fixed arity binds *application*: every invocable with a first-class `$name`
form — builtin, prelude function, user binding — takes a fixed number of value
arguments.** There is no variadic application and no defaulted parameter.

Arity is part of a computation's type. Under [[design/cbpv|call-by-push-value]]
a function consumes a fixed sequence of value arguments; a variable count would
make application irregular, and would leave a reified primitive's type
undefined ([[design/builtins|builtins]]).

**A command entry is not applied, so the rule does not reach it.** An external
command, an intercepted operation, and a command-shaped builtin take an *argv*,
not a curried argument sequence. Hence `cd` and the job-control verbs `fg` /
`bg` / `disown` take zero or one argument (`ArgSig::Optional`,
`core/src/typecheck/builtins.rs`); an intercepted operation takes as many as the
call site writes; and `echo` and `detach` take a fully open argv (`ArgSig::Any`)
([[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]).

**Arity decides which mechanism a name is, and nothing declares it.** A
manifest entry's arity is read off its type rule — a signature's argv shape, a
scheme's curry-spine depth (`BuiltinEntry::fixed_arity`) — and that reading is
the whole classification: fixed arity makes the name a first-class *native*
value, an open or optional argv makes it a *base frame* on the handler stack,
whose arguments arrive as an argv like any command's
([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]).
So `$name` exists exactly where a fixed arity does, by construction rather than
by agreement between a declaration and a check.

**The discipline survives at that boundary by packing, not by banning.**
`run_handler` (`core/src/runtime/command_call.rs`) hands a handler its arguments
as one list value — `[argv]` for a per-name entry, `[name, argv]` for a
catch-all — so a variadic operation is consumed by a fixed-arity lambda typed
`Fun(List α, B)` ([[internals/handler-dispatch|handler-dispatch]]). Variable
arity is a surface phenomenon; at the value level it is always a list.

Optionality is data too, never a hole in an argument list: an
[[invariants/optionality-via-variants|open variant]] (`` `some v `` /
`` `none ``), passed as an ordinary value. The caller always supplies every
argument; the *value* it supplies may carry presence or absence.

This is a hard rule, not a stylistic preference. Do not introduce variadic
application, a splat parameter, a defaulted argument on a function or prelude
binding, or a first-class `$name` for an entry whose argv shape is variable.
