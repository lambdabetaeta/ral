---
status: active
generated_at_commit: 2d05ef216180fc2454ccef6f4ed4d4c65bf69136
---

# The error record

**A failure is one reason and one provenance; its text and process code are
projections, not independent facts, and a program reads only the projection.**
This supersedes in part
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
- A reason is Rust's closed taxonomy of causes. A program that branches on it
  binds itself to that taxonomy, and the checker would carry its whole type.
  A program needs a wording and a status; the reason serves the wire, logs and
  hosts.
- Unwind notes encoded in hint strings suppress one another and cannot count
  discarded bytes. Facts about an unwinding are not advice about a failure.
- A frame name read as the failed program words an external run from
  invisible code with the wrong subject and misses its exit hints. The
  program is a coordinate of the process failure, not of provenance.

## The record and the two rules

```text
Reason  = process [program: String, failure: Failure]
        | timed-out | cancelled | raised Claim
        | denied Denial | io IoFailure | invalid InvalidFailure
        | evaluation EvalFailure | state StateFailure | compile CompileFailure
        | launch LaunchFailure | protocol ProtocolFailure | host HostFailure
        | invariant InvariantFailure
Claim   = [message: String, status: 1..=255]
Failure = exited Int | signaled Int | not-found
        | not-runnable [found: String, why: Unrunnable]
Unrunnable = permission-denied | directory | not-executable
           | missing-interpreter (Option String) | io IoCause

ErrorFact   = [frame: String, reason: Reason, site: CallSite]            -- Rust, Datum
ErrorRecord = [frame: String, message: String, status: Int,
               site: [script: String, line: Int, col: Int]]              -- Datum + Typed
ErrorFact::record : ErrorFact → ErrorRecord
  message ↦ Reason::describe, status ↦ FailureCode::failure_code

fail  : ∀ε α. String → F^ε α
raise : ∀ε α. ErrorRecord → F^ε α

⟨ fail m | κ ⟩ → ⟨ ↯(raised [m, 1], prov κ) | κ ⟩
⟨ raise e | κ ⟩ → ⟨ ↯(reason w, at (frame w, site w)) | κ ⟩         e witnesses w
⟨ raise e | κ ⟩ → ⟨ ↯(raised [message e, status e], at (frame e, site e)) | κ ⟩
                                                                        e hand-built
```

- **`Reason` is the one sum, a Rust-side fact.** It lives on runtime `Error`
  and in `ErrorFact`, the wire's and the logs' failure
  (`core/src/fact/error.rs`). It is `Datum` but never `Typed`: it does not
  reach the checker. Process-layer failures convert once, keeping the command
  head as dispatched as `program`; not-runnable detail includes the path found
  (else the program), with typed `IoCause` retaining OS evidence. Operational
  failures retain closed subjects and causes; only explicitly authored text is
  `Raised`. Large reason payloads are boxed so runtime `Error` stays below 128
  bytes. `Error::new(reason: Reason)` accepts no implicit string conversion.
- **A program reads the projection.** `ErrorRecord` is `ErrorFact::record()`;
  its `status` is a `NonZeroU8`, so it is a failure code by construction.
  `message` and `status` are derived from the reason and never stored beside
  it: no type holds both a `Reason` and a `message`. `Done = Ending<(),
  ErrorRecord, CancelCause>` carries the record to hosts and done events.
- **`Raised` is the program's claim.** `fail m` raises `Claim { m, 1 }`; a
  hand-built record raises its own `Claim` with its own frame and site. Its
  wording is its message and its code is its status.
- **One code table is `FailureCode for Reason`.** `process` delegates to
  `Failure`: `exited n` clamps through `failure_byte`; `signaled s` gives
  128+s; `not-found` 127; `not-runnable` 126. `timed-out` 124; `cancelled`
  143; `raised` its claim's status (1 for `fail`); operational families and
  `invariant` 1. A worker's own stop becomes `timed-out` or `cancelled`
  through `Reason::stopped`; errors, facts and records delegate to the reason
  or, for a record, its stored status. `CancelCause` keeps its own codes
  (aborted 131, …). External raw `i32` survives for wording and hints until
  code projection. Panic 101 belongs to the ending, not the reason. Choosing
  a script code is top-level `exit n`, not `fail`. The table is the shell
  tradition's, not an endorsement: the golden rule may revisit it.
- **`Reason::describe` is the one wording of a reason; the record's `message`
  is that wording.** There is no `describe` builtin: `$e[message]` is it.
  Process wording takes `program` as its subject; provenance never enters it.
  `Error` and `ErrorFact` methods delegate to it, as do diagnostics, audit
  failure text and worker notices. Compact `(exit status N)` appears only for
  timed-out/cancelled workers. A matching `found == program` gets the short
  not-runnable wording; Windows Ctrl-C termination retains its specific
  wording.
- **A caught record witnesses its fact.** To the language the record is an
  ordinary map. Its runtime representation is `Map`'s `Witnessed` variant
  (`core/src/shell/runtime/map.rs`), which holds the `ErrorFact` it projects;
  `Map::witnessed` is the only constructor, so the map is exactly
  `fact.record()`. Reads, equality, printing and patterns cannot tell it from a
  built map. `Value::failure` constructs the witnessed runtime records in
  `try`'s handler argument, `poll` and the audit report's `` `err `` outcome.
  Done events and worker listings instead carry plain `ErrorRecord` data
  through `FOValue`; raising those records makes a claim.
- **`raise` of a witnessed map re-raises the exact reason**, at the fact's
  frame and site, with no validation: the reason was witnessed. Any change
  (insert, spread) builds a new map without the witness, and so does crossing a
  seam as data (`FOValue`, JSON, the wire); a changed record is a hand-built
  record. A worker's own failure crosses as an `ErrorFact`, not as a value.
- **`raise` of a hand-built record validates the claim.** The closed type
  rejects extra fields and wrong field types at check time; data must decode
  to `ErrorRecord`. Status 0 is `InvalidCause::ZeroStatus` (use `return`);
  above 255 is a decode refusal. Otherwise the record is raised as given,
  including hand-built provenance: `[frame: f, message: m, status: 3, site: s]`
  is reported as that claim and yields code 3. An opaque error type was
  rejected: records cross `try`, `poll`, `done` and the wire as ordinary data.

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

- The reason determines ordered intrinsic advice, including signal advice and
  operational remedies; the renderer also looks up the session exit-hints entry
  for `exited`. These are recomputed at each render and so survive the `raise`
  of a witnessed record. Everything stored belongs to one unwinding: stored
  hints, context prefixes, witnesses, compile rejections and unwind facts are
  report-only.
- Hints accumulate in attachment order and none overrides another. A denial's
  intrinsic remedy precedes stored hints; other reasons show stored hints,
  report-context advice, then intrinsic and exit-table advice. Contextual advice
  does not survive without the report context (`docs/SPEC.md` §13.1).
- Context applies to every reason without rewriting it.
  `Unwound { abandoned_tail: bool, discarded: u32 }` records abandoned written
  steps and sums capture bytes saturatingly; escapes carry no such facts.
- One renderer composes the hints, the counted discard note, then the
  abandoned-tail note. The latter is omitted for spawn failures or when a
  hint precedes it.
- A handler's re-raise preserves the record and **keeps every hint the record
  determines**, including the session exit-hints lookup. It drops stored
  hints, context and unwind facts from the first unwinding. A resolved site
  renders without a caret. `await` propagates the live error: no handler
  intervened, so its report facts remain.
- No output bytes, arbitrary payload map or failure aggregation enter the
  record. Audit keeps forensic bytes; domain outcomes are values. Refusals use
  typed causes, not `raised` plus a frame name. A loaded source's `compile`
  summary is intrinsic; its rich source-labelled rejection is report-only.

> **Amended (261107).** Hints accumulate and none overrides another. The
> original text let situational hints override reason hints; that hid true
> advice (a sandbox denial hid the signal advice and the exit-hints entry).
> Path-as-text is a stored hint, written by the evaluator when a grant refuses
> a path head, not a reason hint. The error is one type in every layer
> (`Error`, `core/src/error.rs`): `reason` and `prov` are its only public
> fields, each report fact is private with one writer (`with_hint`,
> `with_context`, `with_witness`, `with_rejection`), `Error::new` is the one
> constructor, and an `Error` enters control flow only through
> `From<Error> for Break`.

> **Amended (261110).** A program no longer reads `reason`. `Reason` stays a
> typed Rust-side fact, carried by the wire and logs as `ErrorFact`; the
> language reads `ErrorRecord = [frame, message, status, site]`, its
> projection. The `describe` builtin is gone (`$e[message]`). `raise` of a
> caught record re-raises its exact reason through the map's witness; a
> hand-built record is the program's `Claim`. The one-sum, one-provenance
> core above is unchanged.

## A refused block is one fact

**A block the runtime vets and the checker refuses is `InvalidCause::IllTyped`:
its role, and every diagnostic's stable code and rendered message.**

```text
IllTyped       = [role: BlockRole, diagnostics: List TypeDiagnostic]   -- non-empty
BlockRole      = handler String | catch-all | hook String | binding String
TypeDiagnostic = [code: String, message: String]                       -- "T0057", rendered
```

- The vetted blocks are a persistent or `within` handler, the `within` catch-all,
  a hook registration and an rc `bindings:` entry. The wording is ``the `git`
  handler does not typecheck: T0057 <message>; …`` (`IllTyped::describe`,
  `core/src/fact/error/invalid.rs`).
- The checker's runtime entry points (handler-arm checking, `catch_all_stands_in`,
  `binding_value_scheme`, `core/src/typecheck.rs`) return every error from
  close, not the first. `TypeError::diagnostic` is the one projection to
  `TypeDiagnostic`, and `ill_typed` (`core/src/shell/runtime/coerce.rs`) the
  one builder. Handlers keep their caret through `Break::locate`.

## Hosts and transport

- The wire ending is `Ending<FOValue, ErrorFact, CancelCause>`
  (`protocol::Ending`) and carries the fact, typed reason included, through
  its strict `Datum` encoding. Exarch's shell report reads that ending.
  `Done` carries the `ErrorRecord`, and `Display::Done` in `record.jsonl`
  uses its encoding. The protocol version changes; there is no compatibility
  for old records, envelopes or scripts.
- `Report::Static` and `Faulted` keep their nonzero host status. A desk
  refusal has no status field: it is an `Error` that crosses either carrier as
  a `WireError` (`reason`, optional report `context`, ordered stored `hints`).
  `WireError::to_error` preserves the reason rather than reclassifying rendered
  text as `Raised`; provenance, witnesses, rejections and unwind facts stay out
  of this envelope.
- Boot answers `settled` settings or `capabilities-refused` diagnostics,
  rendered engine-side so compile reports survive. The host chooses exit 2
  for capability rejection; rc failures remain error endings.
- `exited n` is an ending (the computation ran `exit n`) and, separately, a
  process `Failure`; the reason is `process [...]`, so the two never share a
  sum.

See [[design/failure|failure]], [[design/endings|endings]],
[[design/capture|capture]], [[map/core/diagnostics|diagnostics]] and
`docs/SPEC.md` §8.5–§8.6, §13.
