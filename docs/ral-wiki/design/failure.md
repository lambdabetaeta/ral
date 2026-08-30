# Failure: status, not truth

ral keeps strictly apart two notions a shell conflates. *Failure* is a runtime
condition — a nonzero exit status or a raised error — that propagates
automatically until some form decides what to do with it. *Truth* is an ordinary
[[design/types|`Bool`]] value, inert data. **`?` and `try` react to failure;
`if` branches on truth; the two axes never cross.**

A `Bool` is data, not a verdict. A ral predicate returns a `Bool`, and returning
`false` is a *successful* command: the value is simply the command's value and
raises nothing, so `false` never becomes a failure and never drives `?` or
`try`. Branching on a `Bool` is `if`'s job — it rejects a
non-`Bool` condition as a type error; branching on *success* is `try`'s.

**`?` is a fallback chain: the first success wins.** `a ? b ? c` tries each arm
left to right, returns the first that does not fail, and skips the rest; only a
*failure* falls through to the next arm. A non-failure escape — `exit`, a
job-control stop — propagates out immediately instead of being caught. If every arm fails the last error propagates, and all arms must
share one return type.

**`try` turns failure into data, and is ral's only `||`.** `try B H` runs `B`;
on success it returns `B`'s value, on failure it calls the handler `H` with an
error record `[status, cmd, message, line, col]` and returns *its* value,
unifying both outcomes into one type. It catches recoverable runtime errors and
nothing else — `exit` and job-control stops, the two `Escape`s, bypass it. When a
command encodes its result in its exit status rather than a value — an external
`grep -q`, say — `try` is how that success drives a branch:
`try { grep -q p f; echo found } { |_| echo missing }`. There is no
command-level `||`: it would force precedence against `?` and `|` for a case
`try` already covers. The only `||` is the short-circuiting `Bool` connective
inside `$[…]` expression blocks (`docs/SPEC.md` §17.1), which takes strict `Bool`
operands and never inspects command success.

The cleanup forms differ only in what they do with the original failure:

- `try` suppresses it;
- `guard B C` runs cleanup `C` whether or not `B` failed, and lets `B`'s failure
  keep propagating — unless `C` halts as well, because **any halt of the cleanup
  pre-empts the body's outcome**, an error exactly as an escape. So: `B` returns
  and `C` halts, the halt is the outcome; both halt, `C`'s signal wins, whichever
  kinds the two are; `B` halts and `C` returns, `B`'s signal survives.
  Log-and-continue is one keystroke away — `guard B { try C { |e| … } }` — and
  the primitive stays strict
  ([[decisions/260819_a-failing-cleanup-pre-empts|a-failing-cleanup-pre-empts]]);
- the prelude's `attempt` discards both result and failure.

The kernel model carries both forms as of 2026-08-19. In `dev/agda` a failure is
a term — `halt`, spelled `fail` when it carries an error and `exit` when it
carries an escape — and every stack frame answers one. `try` and `guard` are the
two frames that answer with more than a pop, and the pair of rules `βtry-err` /
`βtry-esc` is the whole of "catches recoverable runtime errors and nothing
else": one pattern match on the signal, catching the error and letting the
escape walk past. `guard` is not sugar for `try` there either — it reduces to
its cleanup resumed in front of the body's outcome (`βguard-halt`, the detour
through `؛`), so a cleanup that halts pre-empts that outcome, which is the rule
above read on the other side: one rule for both signals, and the shell and the
calculus now say it in the same words. `?` needs no form of its own there: it is
`try` with a handler that drops the error it binds, and the model proves the
equation that makes that a definition rather than a resemblance — `fail W ? N`
is `N`.

Failure propagates predictably through the rest of the grammar:

- a sequence `a; b; c` halts at the first failure, and the error says how many
  later parts it abandoned — otherwise the truncation is legible only as a short
  `children` list in an enclosing `audit`, which reads the same as a sequence
  that had fewer parts;
- a [[design/pipelines|pipeline]] fails whole on any stage failure, bar a stage
  ral itself ended because its reader was gone;
- `for` / `map` stop iterating on a failing body;
- `spawn` captures failure in the handle and surfaces it on `await`;
- an unhandled failure at top level exits the process with that status.

See also [[design/syscalls-are-effects|syscalls-are-effects]] (failure is an operation's exceptional outcome),
[[design/pipelines|pipelines]],
[[design/control-operators|control-operators]], [[design/types|types]],
[[design/cbpv|cbpv]].
Cite: RATIONALE §"Failure is not truth", §"Pipelines follow their edges";
`docs/SPEC.md` §2.5, §8, §8.6–§8.7; the `Chain` / `Try` / `Guard` arms and
frames in `core/src/evaluator/machine.rs`, `try` / `guard` typing in
`core/src/typecheck/scope.rs`.
