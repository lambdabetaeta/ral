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
*failure* falls through to the next arm. A non-failure escape — `exit` — propagates out immediately instead of being caught. If every arm fails the last error propagates, and all arms must
share one return type.

**`try` turns failure into data, and is ral's only `||`.** `try B H` runs `B`;
on success it returns `B`'s value, on failure it calls the handler `H` with an
closed error record `[frame, message, status, site]` and returns *its* value,
unifying both outcomes into one type. It catches recoverable runtime errors and
nothing else — `exit` and a stop, the two `Escape`s, bypass it. When a
command encodes its result in its exit status rather than a value — an external
`grep -q`, say — `try` is how that success drives a branch:
`try { grep -q p f; echo found } { |_| echo missing }`. There is no
command-level `||`: it would force precedence against `?` and `|` for a case
`try` already covers. The only `||` is the short-circuiting `Bool` connective
inside `$[…]` expression blocks (`docs/SPEC.md` §17.1), which takes strict `Bool`
operands and never inspects command success.

**A failure is its reason and one provenance; a program reads the projection.** The
[[decisions/261105_the-error-record|error-record decision]] fixes the two rules:

- `fail m` raises the claim `raised [m, 1]` at the innermost visible
  operation, including `fail` itself. `raise e` validates any record, caught,
  changed or hand-built, and raises it as the program's own claim, keeping
  its frame, message, status and site. The record's `message` is
  `Reason::describe`, derived from the reason alone; there is no `describe`
  builtin.
- `frame` names a failing frame, which may be `index`, not a command; it is
  provenance only. A process failure names its own subject: `process [program,
  failure]`, with `program` the command head as dispatched. The reason is a
  Rust-side fact: a program never reads it.
- `frame` is a provenance coordinate: shown with the site, never the subject of
  a process's wording. Visibility
  means a span in a session source; prelude frames defer to the user's call.
  Name and site come from that same frame. Every run supplies a base, so an
  otherwise unclaimed failure is sited at its start, `script:1:1`.
- A raised claim has no typed reason, so it has no intrinsic advice; stored
  hints, context and unwind facts are report-only and are dropped
  (`docs/SPEC.md` §13.1 composes hints and advice). A worker's live error
  carries its completed provenance to `done`, `poll` and `await` alike.
- Operational failures carry typed subjects and causes (`denied`, `io`,
  `invalid`, `evaluation`, `state`, `compile`, `launch`, `protocol`, `host`);
  `invariant` identifies a runtime defect. Only an explicitly authored failure
  uses `raised` text.
- The record's `status` is derived from the reason, never stored beside it.
  Raised text exits 1 (a claim, its own status), operational families and
  `invariant` exit 1; scripts choosing a code use top-level `exit n`.

**An external's failures are raised one way per stage, and blame follows where
the failure arose.** An external's life is `prepare`, `start` and `ending`
([[internals/an-external-end-to-end|an-external-end-to-end]]):

- `prepare` preserves typed authority, input and launch refusals, and its head
  probe finds `not-found` and `not-runnable` before any process exists; `start` converts
  the one `Unstarted` there is, and `ending` the one wait status. These are
  the only builders of a process failure.
- A start that failed in ral's own step, or in a launcher that is not the
  program, is a refusal and never `process [program, failure]`: an error names
  the program only when the program's own `execve` failed
  ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]],
  [[invariants/no-mark-ral-s-fault|no-mark-ral-s-fault]]).
- An `ENOENT` for a file that exists is `missing-interpreter`, not
  `not-found`; only a permission refusal is `permission-denied`
  ([[design/exec|exec]], the outcomes table).
- A confined program's ending is its supervisor's exact word
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).

**Recovery consumes an error; release only observes an ending.**

- `try` suppresses the body's error by returning its handler's result.
- `guard A K R` acquires a resource, calls `K resource`, then
  `R resource end`. `end` is `` `ok | `err ErrorRecord | `exited Int |
  `stopped CancelCause ``; success carries no body value. Returning release
  discards its own value and restores the body's outcome. **Any release halt
  pre-empts that outcome**, error or escape alike
  ([[decisions/260819_a-failing-cleanup-pre-empts|a-failing-cleanup-pre-empts]]).
- An `err` record is the same plain data `try` hands over: `raise $e` in
  release raises it as a claim. No returned value can suppress an ending.
  Log-and-continue belongs inside release, e.g.
  `guard A K { |r end| try { release $r $end } { |e| warn $e[message] } }`.
- Acquisition halting runs no release. Acquisition and release are shielded
  within one grace from the strike; expiry can leak a resource
  ([[decisions/261111_a-guard-acquires|a-guard-acquires]]).
- The prelude's `attempt` discards both result and failure.

Failure propagates predictably through the rest of the grammar:

- a written sequence or `let` halts at the first failure and records whether
  it abandoned written steps. Compiler-generated operand hoists, returned-thunk
  splits and capture decoding set no such fact. The renderer adds the counted
  capture-discard note and the eligible abandoned-tail note after the hints
  (`docs/SPEC.md` §13.1);
- a [[design/pipelines|pipeline]] fails whole on any stage failure, bar a stage
  ral itself ended because its reader was gone;
- `for` / `map` stop iterating on a failing body;
- a loose worker's failure is observed only by `await`, `race` or `poll`; it
  never fails the run or session. Its lifetime is independent of its ending
  ([[design/endings|endings]]);
- an unhandled failure at top level exits with the status derived from its reason
  ([[design/endings|endings]]).

See also [[design/exec|exec]] (failure is an operation's exceptional outcome),
[[design/pipelines|pipelines]],
[[design/control-operators|control-operators]], [[design/types|types]],
[[design/cbpv|cbpv]].
Cite: RATIONALE §"Failure is not truth", §"Pipelines follow their edges";
`docs/SPEC.md` §2.5, §8, §8.6–§8.7; `Break::stopped`; the `Try` / `Guard` arms and frames in
`core/src/shell/evaluator/machine.rs` (`?` elaborates to nested `try`, no frame of
its own), `try` / `guard` typing in `core/src/typecheck/generate/control.rs`.
