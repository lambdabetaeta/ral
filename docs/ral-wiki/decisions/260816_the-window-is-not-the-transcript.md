---
status: accepted
---

# The window is not the transcript

**A block is written to `user.log` once, when it leaves the viewport's window,
and a sync rebuilds only the rows the fold has moved.** The two halves are one
correction: a bounded window may not double as the source of an unbounded
transcript, and a bounded window may not be re-rendered whole per record.

## The window as transcript

[[decisions/260814_one-seam-one-log|one-seam-one-log]] made `user.log` a
regenerable render of the fold's resident blocks, rewritten whole at flush
points. That removed the old incremental tee's data-loss bug and reintroduced
the same shape at the other end: the render reads the *resident* window, so
past eviction the flush wrote a tail and called it the session. Two ways to
lose a transcript, one at each end of the same file.

It cost more than history. `Viewport::evict_to_tombstone` clears a dead
sub-agent's blocks, and `App::flush_logs` reaches those viewports afterwards —
so the flush regenerated an *empty* file over the child's own transcript, which
the tombstone's log path then pointed the user at.

The rule now: the retired prefix only ever grows. `Viewport::enforce_window_caps`
renders what it drops straight into the file, so a block is durable by the time
it leaves heap, and the eviction runs on past any chrome row the dropped rows
anchored and any far half of a fold row that renders as several blocks — both
would otherwise be stranded above a window that can no longer place them, and
would leave the display without ever entering the transcript. The blocks still
resident are written *past* that prefix provisionally by `Viewport::flush_log`,
so `/export` reads a whole session mid-flight and the next retirement rewinds
over them rather than writing them twice. `user.log` therefore keeps the whole
session however long it runs, with the viewport's caps back to what they say
they are: a bound on the screen, not on the record.

Resume appends rather than truncates, and `Viewport::seed` marks the replayed
window as the file's own — the run that recorded those rows already rendered
them, so the continuation joins the transcript instead of repeating its last
thousand rows.

## The window rebuilt per record

`Printer::sync` rebuilt every resident block from the fold on every arriving
record — a fresh markdown parse per block per record, twice over, since
`estimate_rows` renders a block that `reflow` then renders again. With nothing
ever evicted that cost grew all session; with a window it merely stopped
growing, which is not the same as being paid once.

The fold now says when a row last moved. `Blocks` counts its own changes and
stamps each row with that count (`Block::rev`) as it is opened, as the run it
holds grows, and as a result is patched onto it; a printer remembers the
revision it synced at and rebuilds from the first row past it, carrying every
block below over whole — line memo included. Two dependencies reach backwards
past that floor and are named where the floor is computed: a reasoning row's
grain is a fact about the answer run beneath it, so a growing run reopens the
row above; and the most recent `ral` script an answer's echo signal reads
against is a fact about the rows below the floor, which are not being walked.

A row the viewport's own window has already evicted is never built again
(`Viewport::evicted_through`) — the fold's window is the wider of the two, and
building five hundred rows in order to drop them was most of what the old sync
did once a session ran long.

## What follows, and what does not

A block's fidelity is now stamped by the turn that built it rather than
restamped by every later sync. That is what
[[decisions/260618_tui-transcript-as-graphic|the rail's own doctrine]] says
context pressure *is* — turn-level, inherited by every paragraph of a stressed
turn — so the incremental rebuild corrects the reading rather than approximating
the old one.

Two things are deliberately left. The flatten is still rebuilt whole when
stale, bounded by `VIEWPORT_MAX_ROWS`: it is per frame rather than per record,
and with blocks carried over it re-wraps nothing. And `fidelity::context_floor`
still grades a block against *cumulative* session input rather than the last
turn's prompt, so a long session reads as maximum pressure throughout — a
separate wrongness, named here so it is not mistaken for this change's doing.
