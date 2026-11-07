---
generated_at_commit: f1b0a4a4
---

# Endings

**How a computation ended is one generic sum; every observer is a projection.**
Lifetime and failure are independent obligations. A loose worker's ending is
its own, not an implicit failure of the run or session that started it.

## The type and its projections

```text
Ending<V, F, S> = Ok(V) | Err(F) | Exited(u8) | Stopped(S) | Panic(String)
End              = Ending<Value, Error, Stop>
Done             = Ending<(), ErrorRecord, CancelCause>
protocol::Ending = Ending<FOValue, ErrorRecord, CancelCause>
```

- `Stop { cause, owner: Option<ScopeId>, at: Option<Prov> }` retains the
  scope that may consume a stop and the location it unwound through. Errors
  and stops stay distinct because observers consume them differently.
- `Settled<T> = Result<T, Break>` remains the machine's currency in flight,
  not another ending. `End::into_settled` returns or raises; a panic resumes
  its unwind. `Ending::outcome` separates the ok/err half from the halts,
  typed `Ending<Infallible, Infallible, S>`; `widen` embeds them back.
- One tag table names `ok`, `err`, `exited`, `stopped`, `panic` for data and
  checker labels; serde gives the corresponding kebab-case variant names.
- A caught run panic is `Ran { ending: Panic(text), … }`, status 101
  (`PANIC_CODE`), with the checkpoint restored and capture and trail discarded.
  `Report::Faulted` means only that the report was lost: what ran is unknown.
- The wire error is the `ErrorRecord` itself, serialized through its
  first-order encoding: `[reason, frame, site]`, with a total site and no stored
  status or derived message. The wire and `Done` share `F`. A non-data result
  (handle, block or function) becomes `Err`, code 1, completed from the run's
  retained base and rendered with its stored hint.
  Exarch's exit tip reads the record's `reason()`.
- `Report::Ran(Ran { ending, rendered, single_command, captured, trail })`
  holds rendering and run context outside the cases; hosts pass the `Ran`
  whole. `Ok` has no redundant status; command-exit wording is derived.

### Who consumes what

| Observer | `ok` | `err` | `exited` | `stopped` | `panic` |
|---|---|---|---|---|---|
| Run top (`End`) | value | error | byte | `Stop` | text |
| Wire ending | `FOValue` or boundary `err` | record | byte | cause | text |
| `poll`'s `ready.outcome` | data | record, including a converted own stop | re-raise | foreign: re-raise | resume unwind |
| `audit B` | data | record | pass, drop trail | pass, drop trail | unwind |
| `await`, `race` winner, brood join | return | raise | raise | raise | resume unwind |
| `first-settled`, `is-done` | do not inspect ending | same | same | same | same |
| Worker `done`, `workers` row (`Done`) | Unit | record | byte | cause | text |
| Batch `--audit` | value | record | byte | cause | text |
| Batch result | silent, 0 | rendered, failure code | silent, chosen code | rendered, cause code | rendered, 101 |
| Batch loose-worker `done` | silent | silent | silent | silent | silent |
| Last returning run's loose-worker join | drop | drop | drop | drop | drop |
| REPL line | print value | rendered | end session | rendered | rendered |
| REPL loose-worker `done` | next-prompt claim gate, `Done::said` | same | same | same | same |
| Boot | `settled` settings or `capabilities-refused` diagnostic (host exit 2) | rc failure: rendered, derived code | code | rendered, code | rendered, 101 |
| Exarch tool result | VALUE, 0 | rendered; exit tip iff reason is a process `Exited` | chosen code | rendered; timeout tip iff `TimedOut` | rendered + panic tip |
| Exarch loose-worker `done` | admission claim gate: card and wake together | same | same | same | same |
| Synod `Display::Done` | unnarrated | same | same | same | same |
| Pipeline collector | last value | ranked verdict | tear siblings down, rank | forgive by stage name; foreign: tear down, rank | tear down, resume at fold |

A worker base converts an in-session `Cancelled`/`TimedOut` stop to its own
error before these observations; foreign stops remain escapes
([[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]]).
The REPL hook `Outcome` remains a host coarsening of reports and transport
loss, not a parallel ending: it folds errors, panics and lost reports into
its hook-fault policy.

## One rendering, one status

**`End::render` supplies the diagnostic; hosts print it verbatim.** One
private `Diagnosis` feeds one renderer, borrowing the session's `ExitHints`,
with error and stop constructors
supplying fields rather than constructing an error just to draw a stop.
`Ok`, `Exited` and reader-gone stops draw nothing; `Err` and other stops draw
carets or compact reports; `Panic` draws spanless `run panicked: …`.

**A failure that reports success is unrepresentable.** Two traits split the
family by whether success is in range. `FailureCode::failure_code(&self) ->
NonZeroU8` belongs to what can only fail: `CancelCause`, `Stop`,
`Failure`, `Reason`, `Error`, `ErrorRecord`.
`ExitCode::code(&self) -> u8` belongs to what may succeed: an `Ending` (`0`,
the failure's code, the chosen exit byte, the cause's code, or 101) and a
`Break`. `Report::code` delegates for `Ran` and reads the nonzero status of
`Static`/`Faulted`. The process edge is `std::process::ExitCode::from(u8)`.

| Integer door | Rule |
|---|---|
| External failure | retain raw `i32` in `Failure::Exited`; `failure_byte` clamps into `1..=255` when deriving the failure code |
| `exit n` | refuse outside `0..=255` |
| `fail m` | `String`, reason `Raised(m)`, code 1 |
| `raise e` | closed record, trusted unchanged; refuse a failure of `exited 0` and signals outside `1..=127` |

- The clamp's floor is **1**, not 0: Windows `NTSTATUS` read as `i32` may be
  negative, and a failed command must not become success. `Failure::Exited`
  keeps its raw `i32` for the message and the denial/limit hints; those hints
  read the raw reason, never `code`.
- **One failure-code table lives on `Reason`.** `Error`, `ErrorRecord` and
  process failures delegate to it, `process` through `Failure`: `exited`
  clamps, `signaled` gives 128+s, `not-found` 127, `not-runnable` 126,
  `timed-out` 124, `cancelled` 143, `raised` 1. `Reason::stopped` maps the two
  causes a worker converts, `Cancelled` and `TimedOut`, onto their reasons, so
  a converted stop takes its code and words from the table. A panic belongs to
  the ending, code 101.
- Stored host rejection statuses (`Rejection.status`, `Report::Static` and
  `Faulted`) remain `NonZeroU8`. A desk refusal is plain `raised`, with no
  status field. Serde and data's `NonZeroU8` decoder refuse 0; data's `u8`
  decoder refuses integers outside `0..=255`. Neither is another clamp.
- `exited n` is an ending (the computation ran `exit n`) and, separately, a
  `Failure` of a process (`process [.., failure: exited n]`); it is not itself a
  `Reason`. See [[decisions/261105_the-error-record|the error record]].
- Command-audit `fact::Command.status` is `u8`, since a command can succeed;
  the two command-audit observers read `Break`'s `code`.
- Signal handlers call `failure_code` without allocation or locking, widening
  only at `_exit`/`ExitProcess`. `Failure::Signaled` projects `128 + n` through
  `failure_byte`; `Signal::of_status` decodes a status to a signal only where
  a supervisor left no word
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).
  The utility subprocess's own `i32` clamp in
  `core/src/uutils.rs` is outside the shell ending family and is unchanged.

## Unit and the two encodings

**A Unit variant payload is absent.** It is enforced at the two doors:
`datum::tag` (and `Value::variant`) writes it absent, and `datum::untag`, the
one read door, reads an explicit Unit as absent. Nullary decoders therefore
match absence alone; payload decoders read absence as Unit. Other payloads
still must match their type. No decoder, `Ending`'s included, needs a Unit
special case.

- **Wire:** generic serde is externally tagged in kebab-case:
  `{"exited":3}`, `{"stopped":"timed-out"}`. `Ending<(), …>::Ok(())`
  serializes as `{"ok":null}`; the actual protocol alias carries `FOValue`,
  so its Unit result is `{"ok":"unit"}`. An error carries the record.
- **Data:** `Datum` gives payload-less `` `ok ``, `` `err record ``,
  `` `exited 3 ``, `` `stopped cause `` and `` `panic text ``. `Display::Done`
  stores core `Done` through `data::datum::as_datum`: serialize the encoded
  `FOValue`, deserialize then `Done::decode`. For example its Unit outcome is
  `{"variant":{"label":"ok","payload":null}}`, not serde's `{"ok":null}`.
- Batch `--audit` projects the whole wire ending with `map` and `Datum`,
  without re-decoding a record or inventing a fallback. Its legible JSON is
  a separate presentation: `{"tag":"exited","payload":3}` and
  `{"tag":"ok"}` for Unit. `audit B` still hands over only ok/err.

No reader decodes one ending encoding as the other. Old wire and
`DoneOutcome` session-record shapes need not decode; no compatibility is owed.

## Three lifetimes

| Work | Does its failure count? | When it stops |
|---|---|---|
| Brood: a gather or worker body's children | yes (I4) | owner joins on return, cancels and drains on halt |
| Loose session `spawn`/`watch` | only explicit `await`, `race` or `poll` observes it | returning batch waits; halting session takes it down |
| External `detach` | no | outlives the session |

**Lifetime ⟂ failure.** `Run.last` puts the session's wait inside the run's
frame, before its closing poll. Only a returning body enters it. `outlive`
uses `claim_first` and `replay`, not a second wait loop: claim still-unclaimed
session workers in birth order, write stdout then stderr, drop all five
endings. The script retains its own value and status. Cancellation during
the wait or a replay failure can still halt the run.

Taking workers down on return makes `spawn { write-report }; echo done` a
timing race with silent data loss. Waiting gives the written program a stable
lifetime without making its independent workers an implicit success condition.
A batch worker failure emits no failure notice or diagnostic and never changes
the run's status; its written bytes are replayed. Halts (`exit`, uncaught
failure, stop, panic) skip the wait; session teardown takes workers down
silently in batch. REPL quit keeps its existing taking-down line. Exarch
session end is also a halt.

The cost is explicit: a forgotten `spawn { serve }` holds a returning script
open. Cancel work no longer needed, use `exit` or Ctrl-C to halt, or use Unix
`detach` for an **external** meant to outlive the session. A ral block has no
outlive form.

## A notice observes claim state, not future intent

`WorkerState::unclaimed` is `Running | Ready(_)`. `Stopping` is claimed; a
consumed worker has no row, so absence also means claimed. An announcing host
requires a present unclaimed row **when it observes the notice**; the one gate
is `DoneEvent::unclaimed_in(&rows)`:

- REPL holds `DoneEvent`s until `Session::iterate` reaches the next prompt
  (not inside prompt rendering), probes once, then prints one
  faint `note: worker #id (title) …` per still-unclaimed result. A failed probe
  announces nothing.
- Exarch gates at batch admission: only an unclaimed result gets both the
  `Display::Done` card and the model wake. A failed probe announces nothing.
  Other surfaced values replay even when the `done` is suppressed.
- Batch, `-c` and stdin scripts silently consume `done`; they announce none.

A claim before that check suppresses the announcement. A later `await` cannot
retract an announcement already made at an earlier prompt or admission.
`Done::parts`/`said` supplies every host's words: `returned`, `failed: …`
(`ErrorRecord::describe`), ``ran `exit 3` ``, the cause's message, or
`panicked: …`. A worker is not a process; its failure is not worded `exit N`.

## Shapes not taken

- A concrete engine enum plus host enums, or `Settled` plus panic everywhere:
  parallel copies survive; three payload parameters are sufficient.
- Convert every stop to an error record: foreign stops would become data to
  `try` or `poll`, erasing the consumption boundary.
- A sixth wire-only non-data case: failure at the boundary is `err`.
- Rendering inside each case, duplicate command-exit flags or an always-zero
  success status: these are run context or derivable facts.
- Batch `exit ↦ ok ()`, or treating panic as no ending: the report lies.
- Replay every completion card but gate only the wake: a claimed result is
  still spuriously announced.
- A second `_outlive` dispatch: its report would need merging; the wait and
  its bytes, cancellation and trail belong inside the last run.
- A status newtype for the entire family: success is in range for an ending
  and out of range for a failure, so `u8` and `NonZeroU8` behind two traits,
  not one type. Keep no inherent or const `code`; enum matching is
  signal-safe without it.
- A separate wire `Failure { record, status }`: it stores the status twice and
  re-decodes the record to read its reason.
- Accept an explicit Unit payload in each nullary decoder: thirty copies of
  one rule; `untag` is the door.
- Clamp `exit`: silently changes a chosen number. Its door refuses.
  `fail` takes text, not a number; `raise` reports the record's claim.

## Where

The code is mapped in [[map/core/engine-protocol|engine-protocol]],
[[internals/a-turn-end-to-end|a run, end to end]],
[[invariants/a-worker-ends-with-its-children|a-worker-ends-with-its-children]],
[[map/exarch/cards|cards]] and `docs/SPEC.md` §8, §11, §13.4, §16.3.
