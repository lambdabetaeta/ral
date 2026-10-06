---
status: active
generated_at_commit: 3c8afbc3
verified_at_commit: 3c8afbc3
anchors: [Change, Diff, change_card, encode_edit, value_to_edit, OpenedWrite, settle, admit_change, changes_body]
---

# A file change is one fact

**What a write or an edit did to a file is one fact, a `Change`: the path, how
it settled, and its diff. The transcript shows a run of changes as one patch,
each file once, every file at every rung.** The diff is taken where both texts
already stand — in the edit builtin, which holds them whatever the file's
size, and in exarch's decoder, from core's write snapshots — and it is cut at
the source: at most 2000 rows, under a 250 ms search, every changed line
counted. The record carries that diff, never the bytes it came from.

## Context

The 2026-06-20 amendment to
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] made
every write a barrier block of its own, "two writes to one path being two
facts, never merged". A loop appending twice to each of nine files drew
eighteen `write X committed` blocks, none saying what it wrote. Underneath,
one idea had three presentations and two merge rules:

- a `>` drew the new file against the empty side, though core had read the
  old one: an overwrite read as a creation, and lines touched counted the
  whole file;
- a `>>` or `>~` was reported at its open, before a byte landed, with nothing
  to show;
- an edit surfaced an uncapped diff card, and consecutive ones merged by
  concatenating hunks taken against different bases — a block that was the
  diff of no two files.

## Decision

- **Core reports every write when its frame settles, with both sides.** An
  opened target is an `OpenedWrite`, `Atomic` (staged beside the target) or
  `Stream`. Its before-image is taken at the open, its after-image at settle —
  the staged file for an atomic `>`, read before the rename, the target itself
  for a stream — each whole or `none`: past 64 KiB, not a regular file, or not
  readable under the live grant. A stream stays `committed` whatever the body
  did ([[decisions/260930_redirects-are-bindings|redirects-are-bindings]]);
  only its report moves, so it can carry what landed. An atomic write that did
  not land carries neither side.
- **One fact in exarch.** `Change { path, outcome, diff }`, and `Diff::between`
  is the one place two texts become hunks. `decode_surface` turns a write
  observation into its change (`Change::of`) with no diff unless both sides
  are known text, since an unknown before-image must not read as a creation.
  The edit builtins diff the texts they hold and surface `encode_edit`;
  `atomic_write` still reports nothing, so an edit is never counted twice.
- **Cut at the source, then at display.** A diff keeps 2000 rows; its search
  and its inline emphasis share one 250 ms deadline; `added`/`removed` count
  every changed line. `Display::Change` records it.
- **A run of changes is one block.** A change joins the `BlockKind::Changes`
  standing at the mirror's tail (`Block::admit_change`) or opens one; effects
  pass over it back to their call, and the next call opens a new block, so a
  run is in practice one call's writes. Files appear in the order first
  touched, each file's changes stacked under one header — `diff <path>` with
  its size and grain, or `write <path> <outcome>` with no diff to show — and a
  write that did not commit says so. `Tally` is one header per file, `Summary`
  (the opening rung) adds the run's first `DIFF_PEEK_ROWS` rows, `Full` every
  row the source kept and `⋮ N more changed lines` where it cut. A run is
  never folded into a call's tally: a write is still the audit trail.
- **Headless and synod** draw one change at a time, through `change_card`.

## Rejected shapes

- **A net diff per file** — first before-image to last after-image. The cleaner
  picture for a loop that rewrites one file, but the frontend would need both
  whole texts: bytes in the record for every write, or patch composition over
  hunks. Stacking keeps each write visible inside its file and costs nothing.
- **Edits through `atomic_write`'s snapshots.** One producer, but the 64 KiB cap
  would take the diff from the large files edits most touch — the parser, the
  typechecker. An edit always shows its diff.
- **Edits handing both texts to the host to diff.** One diff site, but every
  edit would carry two whole files across the surface, and over the VM channel.
- **Merging only the bodiless writes.** Quiets the eighteen blocks and keeps
  three vocabularies.

## Consequences

- An overwrite reads as the change it made, and lines touched counts changed
  lines.
- An append shows the lines it added, with context; onto a file past 64 KiB,
  its header alone.
- No change puts more than 2000 rows into `record.jsonl`; a run opens at ten
  rows on screen.
- ral's `audit` trail carries both images for every write mode.
- An edit is still invisible to ral's own `audit` trail, since `atomic_write`
  reports nothing; noted, not changed here.

## Where

`core/src/runtime/command/redirect.rs` (`OpenedWrite`, `snapshot`),
`core/src/evaluator/redirect.rs` (`settle`); `exarch/src/bus/card/change.rs`
(`Change`, `change_card`), `exarch/src/bus/card/diff.rs` (`Diff::between`),
`exarch/src/bus/card/encode.rs`/`decode.rs` (`encode_edit`/`value_to_edit`),
`exarch/src/shell_eval.rs` (`decode_surface`), `exarch/src/tui/block.rs`
(`BlockKind::Changes`, `Block::admit_change`), `exarch/src/tui/diff.rs`
(`changes_body`).
