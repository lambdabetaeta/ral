# Fixed arity

**Fixed arity binds *application*: every invocable with a first-class `$name`
form — builtin, prelude function, user binding — takes a fixed number of value
arguments.** There is no variadic application and no defaulted parameter.

Arity is part of a computation's type. Under [[design/cbpv|call-by-push-value]]
a function consumes a fixed sequence of value arguments; a variable count would
make application irregular, and would leave the η-expansion of a reified
primitive undefined ([[design/builtins|builtins]]).

**A command entry is not applied, so the rule does not reach it.** An external
command, an intercepted operation, and a command-only builtin take an *argv*,
not a curried argument sequence, and have no first-class form to η-expand.
Hence `cd` and the job-control verbs `fg` / `bg` / `disown` take zero or one
argument (`ArgSig::Optional`, `core/src/typecheck/builtins.rs`); an intercepted
operation takes as many as the call site writes; and `detach` alone takes a
fully open argv (`ArgSig::Any`) — there is no `$detach` whose η-expansion an
absent count could leave undefined
([[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]).
The registry keeps the two apart structurally: an entry declares `arity: _`
exactly when its signature has no fixed structural arity, and `$name` exists
only where that signature declares a value form (`core/src/builtins.rs`).

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
