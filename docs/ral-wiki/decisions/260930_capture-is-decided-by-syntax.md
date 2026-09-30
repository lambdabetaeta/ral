---
status: superseded
superseded_by: decisions/260930_graded-f
generated_at_commit: 8d868e18
supersedes: decisions/260601_modes-equality-constrained-shared
---

# Capture is decided by syntax

> Superseded by [[decisions/260930_graded-f|graded-f]]: capture is placed by the type, not by syntax.

**`let x = hostname` binds the text `hostname` writes because the checker wraps
the command in the capture coercion, and it decides that once, before any type
is inferred, from the syntax of the right-hand side and the class of its head.**
A command is a computation of type `F Unit`: it writes, and returns nothing.
There is no payload route, no writer type, and no run-time frame that decides.

## What was decided

- **A command is `F Unit`.** An external, a builtin that writes (`Output::Writes`,
  saturated), and a handler arm standing in for one are all `F Unit`, like `cd`.
  `{ echo hi } : {Command Unit}` and `{ 'hi' } : {Command String}`: the checker
  tells a block that prints from one that returns, because the program does.
  `CompTy::Return` carries only its value type; `PayloadRoute`, `PayloadVar`,
  `GroundRoute`, `RouteMismatch` (T0012), `route.rs` and `route_solver.rs` are
  gone.
- **`⟦·⟧` is an elaboration of `let`.** On the right-hand side of a `let`, and
  in the positions its *result* comes from — a sequence's tail (hoisted binds
  included), a pipeline's final stage, a forced literal block, and the arms of
  `if`, `case`, `try`, `within`, `grant` and `guard`'s body when they are
  literal thunks — a command that `writes` is wrapped in `cap M to d. decode d`.
  Every other node is a leaf: an application of a bound name, `!$t`, a value,
  an index, an interpolation, `audit`, and a `Redirect` frame. `capture_sites`
  (`typecheck/capture.rs`) records the commands before the right-hand side is
  inferred; the `Exec` arm answers `F String` for a recorded node after typing
  the call `F Unit`; `annotate` rebuilds structurally and wraps them with
  `captured_string`.
- **A head has one class** (`HeadClass`, by the lookup order checker and
  runtime already share): a binding, a value row (`Returns` or `Writes`), a
  handler arm standing in for a head, or an external. `head_writes(c ā)` holds
  when `c` is no binding and either not a value row or a `Writes` row applied
  at its arity, so an under-applied `to-json` is a function, not a write.
- **A pipeline stage writes.** Every stage but the last is `F Unit` and its
  head is no value row that returns (`PipelineStageWrites`), so `'3' | cat`,
  `length $xs | cat` and `cat f | from-json | wc -l` are refused; the pipeline's
  value is its final stage's. `PipeYield` and `Pipeline.yields` are deleted.
- **An arm stands in for what it is.** `standsFor(c)` is the scheme of the
  base frame or arm already in force under `c`, and `[String] → F Unit` for
  anything else. An arm for `curl` writes and returns `()`; an arm for `detach`
  returns `detach`'s receipt. `Reason::StandsIn` replaces the route pins, and
  `alias_arm_scheme` and the catch-all vet apply the same check at install.
  `detach` refuses a handled head: a handler runs inside this session, so
  nothing could be detached.
- **`{ … }` is a thunk in every position.** `if` and `case` take thunks for
  arms and force the one they choose (`CompKind::If` and `CaseArm` carry a
  `Spanned<Val>`; `ArmBody` is deleted), so `if true $aaah else $baaah` runs
  `$aaah`. An arm is a block or a name; any other atom would be hoisted and run
  before the form chose, and is refused at elaboration.
- **The machine decides nothing.** `Io.ambient`, `swap_ambient_stdout`,
  `with_ambient_stdout`, `write_ambient`, `Frame::To`'s saved stdout, the
  ambient tee of `audit`, and `bytes_promise_broken` are deleted. The capture
  frame keeps its buffer, its flush on failure (to the sink it replaced, where
  it used to be the ambient one) and its 16 MiB cap, and ignores its body's
  value.
- **Two hints keep the cost visible.** An arm join of `()` against another type,
  where the `()` arm's tail is a command, says to capture it (`ls | from-line`)
  or to print in both. A name that `let` bound to what a call returned, when it
  is `()` and is refused as a word or an interpolant, says that the call returns
  `()` and what to write instead.

## Why

Whether `let x = hostname` binds is a fact about the program: there is a `let`,
and its right-hand side is a command. A type variable cannot know it earlier
than the syntax does, and a run-time frame cannot know it better. Routes put
the fact into a fifth sort of variable whose subsumption had two solutions
(`{ |t| if true { echo hi } else { !$t }; let w = !$t; $w }` typed differently
in two statement orders); capture by continuation put it into the stack, where
every frame had to declare a demand and every consumer read a third terminal.
Both reconstructed the one bit the `let` already carries.

The elaboration is total, syntactic and decided once, so the verdict and every
printed type are independent of statement order. Every clause but the first two
is a law of the kernel: β for a literal thunk, associativity of `to`, the fact
that a form which forces one of its thunks has that thunk's result, and the fact
that a pipeline's value is its final stage's. The kernel (`dev/agda`) proves
them for the route-free calculus: type soundness (`soundness`, `progress`);
capture, in two halves — a terminating `M` has `capture M` return exactly the
bytes `M` wrote and write nothing itself, a halting one halts with the same
signal after writing what `M` wrote (`capture-returns`, `capture-halts`);
η for `F`, `M to x. return x ≈ M` (`to-η` in `Bisim`), false of the route
calculus because a write could walk past a capture as chatter; and
associativity and β (`to-assoc`, `؛-assoc`, `force-thunk`). The kernel covers
this ruling only: it has no kinds, rows, weak variables or boundaries.

## What it costs

- **A function's writes are never a bind's value.** `let x = f` binds what `f`
  returns; if `f = { hostname }` that is `()`, and the name prints. The fix is
  at the definition (`{ hostname | from-line }`) or at the call
  (`let x = f | from-line`). The same holds for `!$t`, a wrapper
  (`let x = time { ls }`), a block with a redirect, and a pipeline whose final
  stage is a bound function. A `let` captures the command it can *see*.
- **A value-position join of a command with a String is refused** outside a
  bind, with the hint above; inside a bind a command arm is captured, so the
  empty arm of `let x = if $c { ls } else {}` must be written `''`.
- **A stage writes.** A value or block literal in stage position is refused,
  and so is a value row that returns, `fold-lines` included, before the last
  stage.
- **A stand-in writes; it does not return text**, and everything it writes is
  captured (`curl: { |a| echo note; echo body }` under `let x = curl` binds
  `"note\nbody"`).
- **`()` is neither a word nor an interpolant.**

The non-compositional fact the surface keeps is `⟦·⟧` itself: `let x = cmd; N`
is not `cmd; N` when `x` is dead, `let t = { ls }; let x = !$t` is not
`let x = !{ ls }`, and `let x = a | !$f` is not `let x = a | !{ b }`. Each is
the same fact, and it is what `let x = hostname` capturing means.

## Supersedes, in part

- [[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]],
  [[decisions/260807_modes-solved-by-deferred-joins|modes-solved-by-deferred-joins]],
  [[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]]
  and [[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]]'s
  route fingerprint: there is no route to equate, join, preserve or fingerprint.
- [[decisions/260911_an-external-is-a-byte-operation|an-external-is-a-byte-operation]]:
  the route goes; `^name` and "an external writes" stand.
- [[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]:
  its route-survival paragraph. Pipes stay positional; a stage writes.
- [[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]]: the coercion is
  unchanged; who places it changes, from a demand walk to `⟦·⟧`.
- [[decisions/260812_no-value-has-an-optional-argument|no-value-has-an-optional-argument]]:
  its "route pin".

The plan and its review record are `dev/docs/plans/260929_plain-hm.md`.
