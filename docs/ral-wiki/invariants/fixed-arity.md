# Fixed arity

**Every callable in ral — builtin, prelude function, user binding — takes a
fixed number of arguments.** There is no variadic application, and no optional
or default argument anywhere in the language, the prelude, or the builtins.

A computation's arity is part of its type. Under [[design/cbpv|call-by-push-value]]
a function is a computation that consumes a fixed sequence of value arguments;
admitting a variable count would make application irregular and defeat the
clean value/computation separation the type discipline rests on.

Optionality, where genuinely needed, is expressed as data rather than as a hole
in an argument list: an [[invariants/optionality-via-variants|open variant]]
(`` `some v `` / `` `none ``), passed as an ordinary value. The caller always
supplies every argument; the *value* it supplies may carry presence or absence.

This is a hard rule, not a stylistic preference. Do not introduce:

- variadic builtins;
- splat parameters;
- defaulted prelude arguments.
