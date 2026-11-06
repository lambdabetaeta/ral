---
status: active
generated_at_commit: ad2123a08dfd12616ada6318e9740042ab7e9e37
---

# The error record

**A failure is one reason and one provenance; its text and process code are
projections, not independent facts.** This supersedes in part
[[decisions/260830_status-is-the-outcome|status-is-the-outcome]]: the run's
status remains an outcome projection, and the failure's code is derived from
its reason too.

## Why

- A stored status duplicates the reason's Unix encoding and requires an
  agreement check on re-raise. Two isomorphic sums, runtime `Status` and
  data `Reason`, require conversions without adding information.
- A stored message can contradict the reason. Only raised text is data;
  spawn detail belongs in the reason's payload, not in prose.
- Fresh failure and re-raising have different arguments. Giving both to
  `fail` would require type-directed overloading; two names state two rules.
- Independently stamped names and spans can name different frames. Partial
  sites make worker observers relocate one failure at `done`, `poll` and
  `await` instead of observing it.
- Unwind notes encoded in hint strings suppress one another and cannot count
  discarded bytes. Facts about an unwinding are not advice about a failure.
- A frame name read as the failed program words an external run from
  invisible code with the wrong subject and misses its exit hints. The
  program is a coordinate of the process failure, not of provenance.

## The record and the two rules

```text
Reason  = process [program: String, failure: Failure]
        | timed-out | cancelled | raised String
Failure = exited Int | signaled Int | not-found
        | not-runnable [found: String, why: Unrunnable]
Unrunnable = permission-denied | directory | not-executable | io String
ErrorRecord = [reason: Reason, frame: String,
               site: [script: String, line: Int, col: Int]]

fail     : ∀ε α. String → F^ε α
raise    : ∀ε α. ErrorRecord → F^ε α
describe : ErrorRecord → Returns String

⟨ fail m | κ ⟩ → ⟨ ↯(raised m, prov κ) | κ ⟩
⟨ raise e | κ ⟩ → ⟨ ↯e | κ ⟩
```

- **`Reason` is the one sum**, on runtime `Error` and in `ErrorRecord` alike.
  Process-layer failures convert once, keeping the command head as dispatched
  as `program`; not-runnable detail includes the path found (else the
  program), with OS text only in `io`. `Process` is boxed so runtime `Error`
  stays below 128 bytes.
- **One code table is `FailureCode for Reason`.** `process` delegates to
  `Failure`: `exited n` clamps through `failure_byte`; `signaled s` gives
  128+s; `not-found` 127; `not-runnable` 126. `timed-out` 124; `cancelled`
  143; `raised` 1. Worker-stop cases delegate through `WorkerStop` to
  `CancelCause`; errors, records and process failures delegate to the reason. External raw `i32` survives for wording and hints
  until code projection. Panic 101 belongs to the ending, not the reason.
  Choosing a script code is top-level `exit n`, not `fail`.
- **`describe` is the one wording of a reason.** Process wording takes
  `program` as its subject; provenance never enters it. Runtime and record
  methods delegate to it, as do diagnostics, audit failure text and worker
  notices. Compact `(exit status N)` appears only for timed-out/cancelled
  workers. A matching `found == program` gets the short not-runnable wording;
  Windows Ctrl-C termination retains its specific wording.
- **`raise` trusts the record.** Its closed type rejects extra fields; data
  must decode to that type. It refuses a failure `exited 0` (use `return`)
  and signals outside `1..=127`, but otherwise raises the record unchanged,
  including hand-built provenance. A claimed
  `process [program: p, failure: exited 3]` is reported as that claim and
  yields code 3. An opaque error type was rejected: records cross `try`,
  `poll`, `done` and the wire as ordinary data.

## One provenance from the continuation

**`prov κ` is the innermost visible operation frame, the redex's own included.**
Visibility has one definition: the span lies in a session source.

- `frame` names the reporting frame, not necessarily a command, and is pure
  provenance: shown with the site, never the subject of process wording. An
  evaluator refusal names an operation such as `index`; visible `fail` names
  `fail` at that call. The baked prelude is invisible, so `last []` names and sites the
  user's `last`. `_` remains module privacy, not a provenance rule.
- `Break::locate` claims an unset provenance once, with both name and span.
  `Prov` is live name/span (caret rendering) or resolved name/`CallSite`
  (coordinate rendering). Dispatch claims command/native failures; evaluator
  operations claim their own refusals, without fallback `Exec`/`App` stamps.
- One total register tracks the current visible continuation frame and is
  saved/restored with it. The field is private to `Audit`: `enter_site`
  returns an opaque `DisplacedSite` that only `restore_site` consumes, so
  every write is bracketed. A source base installs its root span; a hook base
  installs its origin at 1:1; workers inherit the register at spawn. A failure
  no inner frame claims is sited at its run's start, `script:1:1`; so is a
  result refused at the data boundary, from `RunReport::Ran.base`.
- Record minting reads error provenance, else the register. Audit observation
  sites use the same total register. With no listeners, `audit_start` returns
  `None` without the observation work.
- A child shell forks the source registry; the prefix it inherited stays
  shared. An ending leaving a worker or pipeline stage resolves every live
  provenance past that prefix, since the parent may register another file at
  the same id.
- Stops carry the same provenance. Worker stop conversion preserves it, and
  the worker completes any unlocated error before publication. `done`, `poll`
  and `try` around `await` therefore receive identical worker records.

## Report facts are not the value

- Reason hints depend on the reason and session exit-hints table: signal
  advice, path-as-text advice, external exit hints keyed on `program`. They
  are recomputed, never stored on the error. Situational hints depend on additional context and
  override reason hints.
- Context prefixes, witnesses, compile rejections and unwind facts are
  report-only. Context applies to every reason without rewriting it.
  `Unwound { abandoned_tail: bool, discarded: u32 }` records abandoned written
  steps and sums capture bytes saturatingly; escapes carry no such facts.
- One renderer composes the hint, counted discard note, then abandoned-tail
  note. The latter is omitted for spawn failures or when a hint exists.
- A handler's re-raise preserves the record and **keeps every hint the record
  determines**, including session exit-hints lookup. It drops situational
  hints, context and unwind facts from the first unwinding. A resolved site
  renders without a caret. `await` propagates the live error: no handler
  intervened, so its report facts remain.
- No output bytes, general structured payload, failure aggregation or
  separate `refused` reason enter the record. Audit keeps forensic bytes;
  domain outcomes are values; `raised` plus the frame name describes refusal.

## Hosts and transport

- The wire ending remains `Ending<FOValue, ErrorRecord, CancelCause>` and
  carries the record through its strict `Datum` encoding. `Display::Done` in
  `record.jsonl` uses that encoding too. The protocol version changes; there
  is no compatibility for old records, envelopes or scripts.
- `Report::Static` and `Faulted` keep their nonzero host status. Desk refusals
  have no status field and become plain raised errors.
- Boot answers `settled` settings or `capabilities-refused` diagnostics,
  rendered engine-side so compile reports survive. The host chooses exit 2
  for capability rejection; rc failures remain error endings.
- `exited n` is an ending (the computation ran `exit n`) and, separately, a
  process `Failure`; the reason is `process [...]`, so the two never share a
  sum.
- The Agda halt carries an opaque value; its error payload is unchanged.

See [[design/failure|failure]], [[design/endings|endings]],
[[design/capture|capture]], [[map/core/diagnostics|diagnostics]] and
`docs/SPEC.md` §8.5–§8.6, §13.
