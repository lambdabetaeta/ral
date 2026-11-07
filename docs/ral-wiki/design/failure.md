---
generated_at_commit: ad2123a08dfd12616ada6318e9740042ab7e9e37
---

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
closed error record `[reason, frame, site]` and returns *its* value,
unifying both outcomes into one type. It catches recoverable runtime errors and
nothing else — `exit` and a stop, the two `Escape`s, bypass it. When a
command encodes its result in its exit status rather than a value — an external
`grep -q`, say — `try` is how that success drives a branch:
`try { grep -q p f; echo found } { |_| echo missing }`. There is no
command-level `||`: it would force precedence against `?` and `|` for a case
`try` already covers. The only `||` is the short-circuiting `Bool` connective
inside `$[…]` expression blocks (`docs/SPEC.md` §17.1), which takes strict `Bool`
operands and never inspects command success.

**A failure is its reason and one provenance.** The
[[decisions/261105_the-error-record|error-record decision]] fixes the two rules:

- `fail m` raises `raised m` at the innermost visible operation, including
  `fail` itself. `raise e` re-raises the closed record unchanged and trusts
  its claims. `describe e` derives the text from the reason alone.
- `frame` names a failing frame, which may be `index`, not a command; it is
  provenance only. A process failure names its own subject: `process [program,
  failure]`, with `program` the command head as dispatched.
- `frame` is a provenance coordinate: shown with the site, never the subject of
  a process's wording. Visibility
  means a span in a session source; prelude frames defer to the user's call.
  Name and site come from that same frame. Every run supplies a base, so an
  otherwise unclaimed failure is sited at its start, `script:1:1`.
- Reason hints are recomputed on re-raise; situational hints, context and
  unwind facts are report-only and are dropped. A worker's live error carries
  its completed provenance to `done`, `poll` and `await` alike.
- The exit code is derived from `reason`, never stored beside it. Raised
  text exits 1; scripts choosing a code use top-level `exit n`.

**An external's failures are raised one way per stage, and blame follows where
the failure arose.** An external's life is `prepare`, `start` and `ending`
([[internals/an-external-end-to-end|an-external-end-to-end]]):

- `prepare` raises ral's refusals as `raised` text; `start` converts the one
  `Unstarted` there is, and `ending` the one wait status, so no other path
  builds a process failure.
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
  the cleanup runs shielded against a stop for the run's grace from the strike
  ([[decisions/261016_a-cleanup-is-shielded-within-its-grace|a-cleanup-is-shielded-within-its-grace]]);
- the prelude's `attempt` discards both result and failure.

The Agda core models both as frames on a stack a halt walks:
`try` pops and forces its handler on the error (`err-try`); `guard` runs
its cleanup in front of the body's outcome (`guard` / `restore` frames), so a cleanup that halts replaces that outcome, as above. It has no
`exit`.

Failure propagates predictably through the rest of the grammar:

- a written sequence or `let` halts at the first failure and records whether
  it abandoned written steps. Compiler-generated operand hoists, returned-thunk
  splits and capture decoding set no such fact. Rendering follows the hint,
  counted capture-discard note, then eligible abandoned-tail note; the last
  is omitted for spawn failures or when a hint exists;
- a [[design/pipelines|pipeline]] fails whole on any stage failure, bar a stage
  ral itself ended because its reader was gone;
- `for` / `map` stop iterating on a failing body;
- a loose worker's failure is observed only by `await`, `race` or `poll`; it
  never fails the run or session. Its lifetime is independent of its ending
  ([[design/endings|endings]]);
- an unhandled failure at top level exits with the code derived from its reason
  ([[design/endings|endings]]).

See also [[design/exec|exec]] (failure is an operation's exceptional outcome),
[[design/pipelines|pipelines]],
[[design/control-operators|control-operators]], [[design/types|types]],
[[design/cbpv|cbpv]].
Cite: RATIONALE §"Failure is not truth", §"Pipelines follow their edges";
`docs/SPEC.md` §2.5, §8, §8.6–§8.7; `Break::stopped`; the `Try` / `Guard` arms and frames in
`core/src/shell/evaluator/machine.rs` (`?` elaborates to nested `try`, no frame of
its own), `try` / `guard` typing in `core/src/typecheck/scope.rs`.
