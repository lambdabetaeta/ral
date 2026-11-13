# The exec argv is words

**An argv the shell renders is total but for `()`; an argv crossing into `execve(2)` is
*words*. The shapes that are not words are declared in exactly one place, and
that one declaration is read at both moments a call can be refused — by the
checker before the run, by the spawn at it.**

Total inside but for `()`, gated at the operating-system call. The asymmetry is
not an oversight in either direction:

- **Inside the shell**, `ValClos::render_argv` (`core/src/ir/value.rs`)
  gives every value a text form, so a base frame, a handler arm and the audit
  trail take every shape at run time. `echo [a: 1]` prints a map; `echo $f`
  prints a block. The one shape the checker refuses there is `()`, which is
  nothing and has no text to print: a handler arm or base frame handed a `()`
  argument is a type error (`ArgvBoundary::InShell`).
- **At the boundary**, an argument is one word, and `()`, a list (several
  arguments), a map or record (fields), a block or lambda or native (a
  computation not yet run), a handle (still running), and bytes (a channel) are
  not words. A tagged value renders, so it is not refused.

**One declaration, two readings.** `RefusedArg`
(`core/src/ty/exec_arg.rs`) names those shapes (`Unit`, `List`, `Map`, `Block`,
`Handle`, `Bytes`) by a single wildcard-free match on a `Head`, `of_head`. The
spawn (`check_argv`, `core/src/shell/exec/command.rs`) reads it through
`ValClos::heads`, the checker's argv rule (`gate_arg`, `typecheck/infer.rs`)
through `of_ty`, and
one `remedy` per shape means a user meeting either refusal meets one language. A
new head has to be given a verdict before it compiles, for both sides at once. A second
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
rendering partial beyond `()`, do not gate an in-shell argv beyond it, do not refuse on a type
variable or on a spread's elements, and do not state the refused set a second
time — extend `RefusedArg` and let both sides read it.

See [[design/exec|exec]] for
why an argv is `List String` inside and bytes at the call, and
[[invariants/fixed-arity|fixed-arity]] for the argv/application split the
boundary sits on.
