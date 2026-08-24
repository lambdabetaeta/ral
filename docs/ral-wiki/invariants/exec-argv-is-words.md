# The exec argv is words

**An argv the shell renders is total; an argv crossing into `execve(2)` is
*words*. The shapes that are not words are declared in exactly one place, and
that one declaration is read at both moments a call can be refused — by the
checker before the run, by the spawn at it.**

Total inside, gated at the operating-system call. The asymmetry is not an
oversight in either direction:

- **Inside the shell**, `Value::render_argv` (`core/src/types/value.rs`) gives
  every value a text form, so a base frame, a handler arm and the audit trail
  take every shape. `echo [a: 1]` prints a map; `echo $f` prints a block.
- **At the boundary**, an argument is one word, and a list (several arguments), a
  map or record (fields), a block or lambda or native (a computation not yet
  run), a handle (still running), and bytes (a channel) are not words. `Unit` is
  the word `()` and a tagged value renders, so neither is refused.

**One declaration, two readings.** `RefusedArg`
(`core/src/types/exec_arg.rs`) names those shapes, with `of_value` for the
spawn (`runtime::command::vet`), `of_ty` for the checker's argv rule
(`typecheck::infer`), and one `remedy` per shape so a user meeting either
refusal meets one language. Both maps are wildcard-free: a new `Value` or `Ty`
constructor has to be given a verdict on both sides before it compiles. A second
statement of the set is the failure mode this rule exists to prevent — drift
between them is invisible until a user meets one refusal and not the other.

**Concrete types only, and no spread.** The checker refuses an argument whose
resolved type *is* one of those shapes (**T0057**) and says nothing about a type
variable; and it never refuses a spread, whose element count is dynamic — an
empty spread contributes no argument, so refusing it would reject a call that
spawns cleanly. What the type does not say, the spawn still does (**R0001**).
The static gate is therefore pure gain: **no program that ran before stops
running** ([[decisions/260812_exec-boundary-gated-statically|exec-boundary-gated-statically]]).

This is a hard rule, not a stylistic preference. Do not make the in-shell
rendering partial, do not gate an in-shell argv, do not refuse on a type
variable or on a spread's elements, and do not state the refused set a second
time — extend `RefusedArg` and let both sides read it.

See [[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]] for
why an argv is `List String` inside and bytes at the call, and
[[invariants/fixed-arity|fixed-arity]] for the argv/application split the
boundary sits on.
