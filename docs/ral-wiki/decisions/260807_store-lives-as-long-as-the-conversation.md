---
status: active
---

# The store lives exactly as long as the conversation

**Synod's history store is re-priced to cost what a job actually changed, and
re-scoped to live exactly as long as the conversation that opened it — the two
decided together, because the lifetime is what made the pricing's own
persistence assumptions moot.** The invariant that survives both changes,
unamended: *every byte a job or an undo replaces is in the store before it is
touched.* What changes is how cheaply that invariant is bought, and how long
its receipt is kept.

## Context

The itch was concrete: a 63 GB, 184k-file grant met minutes of unexplained
silence per message, a large-folder warning that only appeared *after* the
wait it warned about, and a 45 GB store left behind for a job that touched
three files. Worse, a capture crashed outright under `cargo clean` on a
folder that size — the first cut over-satisfied the invariant above by
reading and hashing the *whole* folder before the first message and again
after every exchange, and by keeping the store forever on the strength of a
cross-session undo the product never actually offered: the window's report
and undo read the store only while their conversation runs. Durability was
all cost and no surface.

## Decision

**Pricing.** A capture is a stat walk, not a re-read. Each manifest entry
now carries `mtime_ns` beside its hash (`workspace/manifest.rs`); `capture`
(`workspace/history.rs`) compares a file's current size and mtime against
the folder's *latest checkpoint* and reuses the recorded hash, unopened, on
a match. Git's own racy guard is not skipped: a match is trusted only when
the file's mtime is strictly older than that checkpoint's own moment — and
that moment is stamped at the *start* of the reference walk, not its end, so
a file rewritten while the reference checkpoint was itself being taken
carries an mtime at or after the stamp, fails the strictly-older trust, and
is re-read by the next capture rather than wrongly reused forever after. A live folder is tolerated, not
merely read once: a path that vanishes between listing and reading is a
deletion to record (`WalkError::Vanished`, `hash_file` answering `Ok(None)`),
never an error that aborts the whole capture. And the warning now precedes
the wait: `workspace::manifest::measure` is a stat-only pre-walk that feeds
`Opening::large_folder_line` — a free-space sentence off
`workspace::history::free_bytes` (`statvfs` / `GetDiskFreeSpaceExW`) included
— before a single byte of the folder is read or copied. `Conversation::begin`
no longer joins the before-checkpoint at all: it spawns the capture on its
own thread as a `Baseline` (`Pending`/`Ready`/`Failed`/`Crashed`), and
`Conversation::exchange` settles it — joining the thread the first time,
finding it already settled every time after — *before* driving the model,
since the guest must never write into a folder whose baseline is still
being read.

**Lifetime.** The store lives exactly as long as its conversation. While the
window is open the store necessarily holds one deduplicated copy of the
folder plus every byte a job replaced — the baseline references every file,
so no retention budget could ever shrink it below the folder's own weight.
Bounding it was never the answer; ending it is. Every open `HistoryStore`
holds a shared advisory lock on a `lock` file beside it (`flock` on unix,
`LockFileEx` on Windows — one `lock_imp` module per platform);
`Conversation::end` wipes the store after shutdown, and `sweep_stale`, run
once at startup before any conversation can open its own store, probes every
`<slug>/history` directory with a non-blocking *exclusive* lock and removes
only the ones that probe wins — a crashed session's leavings. A live shared
hold always refuses the exclusive probe, so a conversation still open is
never swept out from under itself.

## Rejected

- **A copy-on-write baseline** (APFS `clonefile`, or a VSS snapshot through
  the Windows machine broker). Both would make the one full read cheaper —
  a metadata-only frozen baseline, hashed lazily behind the first exchange.
  Both were parked: they optimise a once-per-conversation event that is
  already priced honestly and named in the opening warning, at the cost of a
  real module, a gated join, and (on Windows) COM ceremony and snapshot
  lifetime, all spent on an opening impression rather than the steady state
  the pricing above already fixed.
- **A size-budgeted archive with whole-job eviction.** Unworkable, not just
  undesirable: the baseline references every file the folder holds, so a
  store with any live, undoable job weighs exactly what the folder weighs.
  A budget could only ever trim *older* jobs nobody asked to keep, never the
  live one — it would look like a retention policy while doing nothing a
  conversation-scoped wipe does not already do more honestly.

## Consequences

- Undo ends with the conversation, stated as a cost rather than hidden as a
  limitation: closing the window is accepting the folder as it stands.
- Each conversation's opening still pays the folder's one full read — now
  warned about *before* it starts, with a free-space sentence, rather than
  discovered as unexplained silence.
- A quiet, unchanged folder of any size checkpoints in a stat walk after the
  baseline, regardless of how large it is.
- `Conversation::begin`'s error surface shrinks: the before-checkpoint is no
  longer among the failures `begin` itself can return, since its capture
  outlives `begin`'s own return and `exchange` reports its failure instead.
- If post-close regret over the hard cutoff turns out to be real, the shape
  is keep-latest-job-only behind the same startup sweep — not a budget.

## See also

[[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]]
(the same sweep-what-a-crash-left-behind shape, one layer down, for the
session disk rather than the history store), [[decisions/260806_exchange-ends-at-fleet-quiescence|exchange-ends-at-fleet-quiescence]]
(what `exchange` waits for once the baseline is settled), [[map/synod|synod]]
(where `workspace::manifest`, `workspace::history`, and `session::Conversation`
sit in the crate).

Cite: `synod/src/workspace/manifest.rs` (`mtime_ns`, `measure`, `Measure`,
`WalkError`, `hash_file`), `synod/src/workspace/history.rs` (`HistoryStore::capture`,
`HistoryStore::wipe`, `sweep_stale`, `sweep_stale_under`, `sweep_one`, `Lock`,
`lock_imp`, `free_bytes`), `synod/src/session.rs` (`Baseline`, `Conversation::begin`,
`Conversation::exchange`, `Conversation::end`, `opening_warning`), `synod/src/main.rs`
(the `sweep_stale` call ahead of `session::prepare`).
