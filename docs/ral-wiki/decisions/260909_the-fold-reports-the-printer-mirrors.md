---
status: accepted
generated_at_commit: ed466ea2
---

# The fold reports, the printer mirrors

**The view fold says what each record did to it, and a frontend keeps a 1:1
mirror of its own memo driven by those reports.** `Blocks::step` returns a
`Delta` — `Opened` / `Grew` / `Patched` / `Quiet` — and `Sink::fact(id, rec)`
replaces the whole-memo sync: a frontend is a `bus::Sink`, itself a fold over
the one log, stepping its own memo and drawing the increment. Nothing
downstream re-derives the memo, so there is no revision to compare, no floor to
rebuild from, and no second window.

## Why a re-derivation existed

`record::Blocks` is already the right intermediate representation: one block
per incident as it arrived, with consecutive records of one lane joined
([[decisions/260814_one-seam-one-log|one-seam-one-log]]'s corrections). But the
seam handed a printer the *whole* memo and asked it to produce the whole
screen, so the TUI's scrollback was a projection recomputed from that memo
rather than a structure of its own. Two mechanisms grew out of that:

- **A revision, to avoid recomputing everything.** `Blocks` counted its own
  changes and stamped each block with the count; a printer remembered the
  revision it synced at and rebuilt from the first block past it
  ([[decisions/260816_the-window-is-not-the-transcript|the-window-is-not-the-transcript]]).
- **A projection over arrival order, recomputed per frame.** Grouping — a
  burst of `ral` work as one dialable object, deliberation hoisted above the
  work it ordered — was a scan at reflow time (`deliberation_end`,
  `observation_run_end`), so the group was never a value anyone held.

## What it cost

Because a sync rebuilt blocks, nothing a block carried could survive one, and
each such thing needed a side table or a bridge of its own:

- **Dial state** lived in a `HashMap<BlockId, Reveal>` beside the blocks, with
  its own eviction rule to keep in step with theirs.
- **Chrome** — the banner, `/help` and `/copy` acks, `/resources` rows, stop
  reasons: rows deliberately never recorded — could not simply sit in the
  scrollback a sync rebuilt, so it lived in a second lane, each row named by
  the highest `Seq` seen when it was drawn, stable-merged back on every sync.
  That merge was the first of `one-seam-one-log`'s four "live display" blockers.
- **Two windows.** The printer trimmed its own (`VIEWPORT_MAX_BLOCKS`,
  `VIEWPORT_MAX_ROWS`) on top of the fold's `BLOCKS_WINDOW`, and had to
  remember how far its own trim had reached (`evicted_through`) so a sync did
  not build five hundred blocks in order to drop them again. Two bounds meant
  two eviction paths, and retirement to `user.log` hung off the printer's.
- **Backwards dependencies at the floor.** A block's rendering could depend on
  blocks below the rebuild floor — a deliberation's grain on the prose beneath
  it, an answer's echo signal on the last `ral` script before it — so the floor
  had to be widened by hand for each.
- **Decoding per sync.** A `Display::Card`'s mark tree was parsed out of JSON
  every time its block was rebuilt.

## What changes

- **`Delta`.** `Blocks::step` is inherent and returns what it did; `Fold for
  View` delegates to it. `push` answers `Opened` for a new block or `Grew` for
  one joined onto the lane the tail already held; `attach_result` answers
  `Patched`, or `Quiet` where the call it names has gone. Ambient facts — usage,
  the model in force, a protocol record — are `Quiet`. `Block::rev` and
  `Blocks::rev` go.
- **`Sink::fact(id, rec)`.** One trait describes the frontend seam: a `Sink`
  owns its memo(s) and steps them, and takes the witnessed record because a
  fold is stepped by the record *and* its locus. The contract says what it
  always meant: a frontend is handed the record *only* to step its own memo and
  act on the delta, and renders from `record::BlockKind` off that memo, never
  from the record vocabulary. Headless keeps one memo per source agent and
  prints the block each `Opened` opened.
- **The TUI mirror.** `Scrollback` holds a `Vec<Block>` built by `fact`:
  `Opened` tail-merges into the group standing at the tail or pushes what the
  incident reads as, `Grew` replaces the text of the block the fold's tail row
  is drawn in, `Patched` stamps the magnitude on the call the patch names. A
  block is a *value*: its dials, its decoded card, its group live in it, and
  nothing rebuilds it.
- **One window.** `BLOCKS_WINDOW` is the only bound. After every step the
  mirror's head is trimmed to the fold's first block — one comparison — and
  what the trim drops is retired to `user.log` on the way out, which is where
  `the-window-is-not-the-transcript`'s growing durable prefix now hangs.
  Chrome is appended at the tail and inherits the expiry of the block it was
  drawn after; chrome drawn before any block — the banner — lives while the
  fold still holds the session's opening block (`Blocks::origin`).
- **Grouping is online.** A group is a *value* holding two parts: the `∴`
  deliberation (each stretch of thinking the fold committed, and the mass of
  the prose it became) and the `▸` run (one `group::Call` per tool call, its
  effects folded onto it as they land). The rule: `Thinking`, a `ToolCall` with
  a stated intent, an observation-origin card, and a turn rule join the group
  at the tail; every other kind is a barrier that pushes its own block and so
  ends the group. Each part carries its own `Detail`, so one group answers two
  dials and `block_at` reports which part a row belongs to. A turn rule inside
  a group is absorbed and draws nothing; outside one it draws the boundary.
- **`Reveal` becomes `Detail`, three rungs.** `Tally` is the numbers alone
  (a run's `|>` effects counted on one line; a diff's header with its size bar
  and grain), `Summary` the representative slice (the tip call's intent, the
  sparkline, that call's effects; a diff's first twenty rows, the rung a diff
  opens at), `Full` the whole thing (every call with intent, bar, effects and
  ral source; the whole deliberation; the whole payload; every hunk). The old
  `Context` rung — "the summary plus three lines" — is deleted: three lines of
  a script is neither a summary nor the thing, and it was the only reason a
  dial needed a kind-aware skip (a deliberation has no such reading, so its
  dial hopped over the rung). A run and a diff cycle `Tally → Summary → Full →
  Tally`; a deliberation and an act cycle `Summary ↔ Full`; the tool-call
  triangle opens at `Full`. `census` is renamed `tally` — one word for one
  concept — and the rungs are named in comments rather than numbered
  `L0`–`L3`.
- **One word for the thinking lane.** `trace` leaves `tui/` except as the `∴`
  shape's legend label, and "run" is reserved for the run of calls: a prose
  block's `continues` says whether it keeps the rail mark, and a deliberation
  is a deliberation.
- **`user.log` is the same rendering.** The separate `log_lines` path, its
  `prev_md` continuation state and its `opens_rail_run` rule go: the transcript
  is the mirror's own rows at the readable width, forced to `Detail::Full`,
  through the same seam rule the screen applies. The provisional flush that
  lets `/export` read a whole session mid-flight is unchanged.
- **The matrix reads the record.** `turns`, `last_is_error` and
  `latest_reply_md` are facts of the fold and are read off it directly.
- **`record::model::Row` becomes `TurnRow`**, and `Display::Context`'s field
  `rows` becomes `turns`, so that in the TUI `Row` names the visual row and
  nothing else. A log recorded before this does not resume; no shim is written.

## What does not change

- The fold, its window, its privacy: `Block`'s constructor stays private, a
  frontend draws blocks it cannot mint, and eviction stays the fold's own and
  unconditional.
- The live open line. `Transient::Token`/`Thinking` still carry only the text
  past the last newline, still drawn *inside* the part that will absorb it, in
  that part's own ink and through the one rendering path — so the record that
  completes a line changes the text and not the picture, and at most one lane
  is ever open. That claim, and the picture-invariance test behind it, are
  `the-window-is-not-the-transcript`'s and survive intact.
- The rail's doctrine
  ([[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]):
  shape for kind, hue for agent, value for magnitude, size and grain in the
  headers, and a block's fidelity stamped by the turn that built it.
- `fidelity::context_floor` still grades a block against *cumulative* session
  input rather than the last turn's prompt — named again here so it is not
  mistaken for this change's doing.

## Accepted losses

- A group is retired whole, keyed by the block that opened it, so it lives
  until every block it holds has left the fold's window rather than shedding
  its oldest member.
- A deliberation that grows while chrome has been drawn beside it grows in the
  mirror's last record-authored block, which is that same group; a barrier
  between the two would leave the increment unplaced. The interleaving is
  exotic — a slash command typed mid-stream — and the design accepts it rather
  than carry a per-member index.
- Headless draws nothing for a `Grew`, because the two lanes that grow — prose
  and thinking — are the two it draws nothing for at all. Its output is
  byte-identical to what it printed before.

## See also

[[decisions/260814_one-seam-one-log|one-seam-one-log]] (amended: frontends are
folds over the one log, stepped by record),
[[decisions/260816_the-window-is-not-the-transcript|the-window-is-not-the-transcript]]
(supersedes its `rev`/floor mechanism and its "deliberately left" paragraph),
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] (the
graphic vocabulary the rungs and rails serve),
[[map/exarch/frontend|frontend]] (the as-built arm),
[[internals/session-record|session-record]] (the seam and the two folds).
