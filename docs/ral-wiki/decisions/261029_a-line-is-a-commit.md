---
status: active
generated_at_commit: 8cc1d0d9
generated_at_date: 2026-10-29
verified_at_commit: 8cc1d0d9
verified_at_date: 2026-10-29
anchors: [Commit, NonEmpty, Seq, Locus, Sum, Cursor, Log::append, Log::complete_len, Log::record_at, Emitter::commit, Emitter::emit, Publish::commit, Signal::Commit, Scrollback::commit, model::resume]
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
{"sum":"c8ef6fe8aca8346d","commit":{"at_unix_ms":1761730000000,"records":[{"Display":{"kind":"answer","text":"One line.\n"}},{"Forensic":{"kind":"cancelled"}}]}}
```

- `RawValue` preserves that value byte-exact across serialization and
  verification; the timestamp belongs to the entire commit.
- `Sum` is the first 64 bits of blake3 as 16 lowercase hex digits. The
  accidental-match probability is 2⁻⁶⁴; 64 bits suffice for detecting damage
  and keep loci small. It is no defence against an editor who can replace
  both value and sum.
- Hex survives JSON tools that cannot exactly represent integers above 2⁵³.

## Torn, corrupt, foreign

**Damage is not a foreign shape.**

- A last unterminated line, or a last line that is not JSON or fails its sum,
  is *torn*: it is never read and resume quarantines it whole.
- Such damage on an earlier line is *corrupt* and refused by file and line.
- Valid JSON of the wrong line shape, or a sealed commit whose records cannot
  be parsed (including an empty list), is *foreign* and refused wherever it
  stands. Damage may coincidentally produce valid JSON of another shape;
  refusing it rather than quarantining it fails closed.
- Resume folds and checks selection before quarantine, so a refused resume
  leaves the file as found. `Log::complete_len` supplies the whole-commit
  extent to both reader and quarantine.

## Addressing

**A record is addressed by `(commit, index)`.**

- `Seq` orders the pair lexicographically; block ids, rewind points and the
  view fold's high-water mark retain their ordering.
- `Locus` adds the line's byte range and sum. `Log::record_at` refuses a range
  no longer sealed by that sum before parsing its records.
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

## Where

- `exarch/src/record.rs`: `Seq`, `Sum`, `Locus`, `Commit`, `Cursor`, `Publish`.
- `exarch/src/record/non_empty.rs`, `record/log.rs`, `record/seam.rs`,
  `record/model/resume.rs`.
- `exarch/src/bus/signal.rs`, `exarch/src/tui/scrollback.rs`.

See [[internals/session-record|session-record]] and
[[map/exarch/frontend|frontend]].
