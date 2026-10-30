---
status: active
generated_at_commit: fc0c9fbb
generated_at_date: 2026-10-30
verified_at_commit: fc0c9fbb
verified_at_date: 2026-10-30
anchors: [Commit, NonEmpty, Seq, Locus, Sum, Cursor, Log::append, Log::complete_len, Log::extent, log::Reader, Reader::record, Emitter::commit, Emitter::emit, Publish::commit, Signal::Commit, Scrollback::commit, context::resume]
---

# A line is a commit

**One line of `record.jsonl` is one commit: a non-empty list of records
appended together or not at all.** A line is the log's unit of atomicity: a
torn last line is never read. A transition can put several facts on one line;
readers receive its records only after the whole line is validated, while
folds still step them individually. This is the unit of a write-ahead log
([SQLite WAL](https://www.sqlite.org/wal.html),
[PostgreSQL WAL](https://www.postgresql.org/docs/current/wal-intro.html)) and
[Datomic's transaction log](https://docs.datomic.com/transactions/transaction-log.html).

## The line

**The seal names the exact bytes of the commit value.**

```json
{"sum":"1315e869d182df50","commit":{"at_unix_ms":1761730000000,"records":[{"Display":{"answer":{"text":"One line.\n"}}},{"Forensic":"cancelled"}]}}
```

- `RawValue` preserves that value byte-exact across serialization and
  verification; the timestamp belongs to the entire commit.
- `Sum` is the first 64 bits of blake3 as 16 lowercase hex digits. The
  accidental-match probability is 2⁻⁶⁴; 64 bits suffice for detecting damage
  and keep loci small. It is no defence against an editor who can replace
  both value and sum.
- Hex survives JSON tools that cannot exactly represent integers above 2⁵³.
- **Records are externally tagged at every level down to a value.** `Transcript`, `Control`,
  `Display` and `Forensic` carry no `tag = "kind"`: an internally tagged enum
  buffers through serde's `Content` and replays it on the native stack,
  bypassing the stack-grown reader, so a deep reply value would make its
  session un-resumable. Types still internally tagged inside records
  (`Settlement`, `DoneOutcome`, `Mark`, diff rows) carry no `FOValue`, so their
  depth is bounded by their type.
- Reader and writer are both unbounded in depth and stack-grown
  (`serde_stacker`); `seal` serializes through an adapter into a `RawValue`
  and never re-parses. A test round-trips a 5000-deep `Control::Status`
  carrying a reply on a 2 MiB thread.
- Several records on one line are groundwork for the durable-agent plan
  (`dev/docs/plans/261028_durable-agent.md`, L3 "one transition, one commit");
  `Emitter::emit` writes single-record commits; control transactions
  use multi-record lines.

## Torn, corrupt, foreign

**Damage is not a foreign shape.**

- A last unterminated line, or a last line that is not JSON or fails its sum,
  is *torn*: it is never read and resume quarantines it whole.
- Such damage on an earlier line is *corrupt* and refused by file and line.
- A shape error is re-checked for well-formedness: a line that is not
  well-formed JSON is damage, not foreign. A well-formed `{sum, commit}` whose
  sum is not 16 lowercase hex digits is a failed seal, also damage.
- A line and its body must be JSON objects; an array is foreign.
- Well-formed JSON of the wrong line shape, or a sealed commit whose records
  cannot be parsed (including an empty list), is *foreign* and refused
  wherever it stands. Damage may coincidentally produce valid JSON of another
  shape, such as a bit-flipped key (`"sun"`); refusing it rather than
  quarantining it fails closed.
- Resume folds context and all three control folds before quarantine, so a refused resume
  leaves the file as found. `Log::complete_len` supplies the whole-commit
  extent to both reader and quarantine.
- `Log::append_to` refuses a log whose tail is not a whole sealed commit
  (`Log::extent`), naming the torn byte range, so an append cannot bury it
  mid-log; resume quarantines first.

## Addressing

**A record is addressed by `(commit, index)`.**

- `Seq` orders the pair lexicographically; block ids, rewind points and the
  view fold's high-water mark retain their ordering.
- `Locus` adds the line's byte range and sum. `log::Reader` (`Reader::record`)
  refuses a range no longer sealed by that sum before parsing its records, and
  parses each addressed line once across consecutive loci on it
  (`Commit::holds`).
- `Cursor` counts whole commits, so catch-up cannot resume midway through one.

## Publication

**One `Signal::Commit` carries the whole commit.**

- The TUI drains bounded passes and draws between them; one signal prevents a
  pass boundary from splitting a commit on screen.
- `Sink::accept` calls `fact` for each record in order within one call. Folds
  step records, not commit boundaries; their intermediate states are not
  indivisible.

## Concessions

**The seal is not a durability or presentation barrier everywhere.**

- Appends do not `fsync`: process-crash recovery is not power-loss durability.
  Several damaged tail lines are refused rather than recovered.
- A hand-edited last line that breaks its sum is quarantined, not refused;
  damage leaving well-formed JSON of the wrong shape is refused, not
  quarantined.
- Synod narrates a commit as consecutive webview events, between which the
  webview may paint. A plan introducing a multi-record commit Synod narrates
  must first batch its events per commit.
- Logs using legacy `{at_unix_ms, record}` envelopes are refused at their
  first line.

## Amendment — 2026-10-30: four classes, one control door

[[design/agents|agents]] names the classes `Transcript`,
`Control`, `Display` and `Forensic`. `Emitter::begin` appends the head and
returns its seeded folds. Every later control line uses `commit_to`: append
and publish, then step borrowed `Mail`, `Schedules` and `Model`, under the
cell lock (or before it exists at reopen). Failed append steps nothing;
ordinary `commit` rejects control records. Reports, fires, clear and reopen
now put related transitions on one line. Synod narrates no `Control` record,
so each such commit yields at most one narrated event; no webview batching
change is needed. A commit introducing two narrated events still requires it.

## Where

- `exarch/src/record.rs`: `Seq`, `Sum`, `Locus`, `Commit`, `Cursor`, `Publish`.
- `exarch/src/record/non_empty.rs`, `record/log.rs`, `record/seam.rs`,
  `record/context/resume.rs`.
- `exarch/src/bus/signal.rs`, `exarch/src/tui/scrollback.rs`.

See [[internals/session-record|session-record]] and
[[map/exarch/frontend|frontend]].
