# Failure: status, not truth

ral keeps strictly apart two notions a shell conflates. *Failure* is a runtime
condition — a nonzero exit status or a raised error — that propagates
automatically until some form decides what to do with it. *Truth* is an ordinary
[[design/types|`Bool`]] value, inert data. **`?` and `try` react to failure;
`if` branches on truth; the two axes never cross.**

A `Bool` is data, not a verdict. A ral predicate returns a `Bool`, and returning
`false` is a *successful* command: the value is recorded in the `$status`
register for inspection but raises nothing, so `false` never becomes a failure
and never drives `?` or `try`. Branching on a `Bool` is `if`'s job — it rejects a
non-`Bool` condition as a type error; branching on *success* is `try`'s.

**`?` is a fallback chain: the first success wins.** `a ? b ? c` tries each arm
left to right, returns the first that does not fail, and skips the rest; only a
*failure* falls through to the next arm. A non-failure escape — `exit`, a
job-control stop, the tail-call trampoline — propagates out immediately instead
of being caught. If every arm fails the last error propagates, and all arms must
share one return type.

**`try` turns failure into data, and is ral's only `||`.** `try B H` runs `B`;
on success it returns `B`'s value, on failure it calls the handler `H` with an
error record `[status, cmd, message, line, col]` and returns *its* value,
unifying both outcomes into one type. It catches recoverable runtime errors and
nothing else — `exit`, job-control stops, and the trampoline bypass it. When a
command encodes its result in its exit status rather than a value — an external
`grep -q`, say — `try` is how that success drives a branch:
`try { grep -q p f; echo found } { |_| echo missing }`. There is no
command-level `||`: it would force precedence against `?` and `|` for a case
`try` already covers. The only `||` is the short-circuiting `Bool` connective
inside `$[…]` expression blocks (`docs/SPEC.md` §2), which takes strict `Bool`
operands and never inspects command success.

The cleanup forms differ only in what they do with the original failure:

- `try` suppresses it;
- `guard B C` runs cleanup `C` and lets the failure keep propagating;
- the prelude's `attempt` discards both result and failure.

Failure propagates predictably through the rest of the grammar:

- a sequence `a; b; c` halts at the first failure;
- a [[design/pipelines|pipeline]] fails whole on any non-SIGPIPE stage failure;
- `for` / `map` stop iterating on a failing body;
- `spawn` captures failure in the handle and surfaces it on `await`;
- an unhandled failure at top level exits the process with that status.

See also [[design/syscalls-are-effects|syscalls-are-effects]] (failure is an operation's exceptional outcome),
[[design/pipelines|pipelines]],
[[design/control-operators|control-operators]], [[design/types|types]],
[[design/cbpv|cbpv]].
Cite: RATIONALE §"Piping and failure", §"No command-level `||`"; `docs/SPEC.md`
§4.4, §10, §10.1–§10.2; `eval_chain` / `eval_return` in
`core/src/evaluator/comp.rs`, `try` / `guard` typing in
`core/src/typecheck/scope.rs`.
