---
status: accepted
---

# A trace is a fold, not a second file

**A session keeps one durable record.** The operational trace that stood beside
it, `transcript.jsonl`, is *deleted outright* rather than retired to a filtered
projection of the log: an operational view is a fold of `record.jsonl` like
every other durable artifact, and a fact the log's vocabulary has no home for is
named a loss rather than kept alive by a second file. This pays in full the
two-record debt [[decisions/260623_recording-follows-the-event|recording-follows-the-event]]
booked and [[decisions/260814_one-seam-one-log|one-seam-one-log]] paid by half.

## Why the second record could not be folded

`one-seam-one-log` unified the *durable* half — the model view and the resumed
scrollback became folds of `record.jsonl` — and left the trace an independently
written file, fed at the bus emit seam outside that change's boundary. That is
still two authorities for one session, which is the shape 260623's debt names;
retiring the trace *to a projection* would have kept the file and only changed
who wrote it. The question a decision has to answer is not where the second
record is written but whether the session has anything left that only it holds.

It has three facts, and each has a truer home in the one log:

- **A per-line clock** becomes the log line's own `Entry.at_unix_ms`
  (`exarch/src/record.rs`). `Entry` is private to the log module — a fold sees a
  bare `Record` — so the clock is a property of the *line*, not of the
  vocabulary, and no projection can come to depend on it.
- **A child's `born`/`died`** are its own `Protocol::SessionStarted` and
  `Protocol::SessionEnded` bookends in its own log, where a child's lifetime was
  always recorded.
- **`stop_reason`** already rides `Protocol::AssistantMessage`; the trace held a
  copy.

## The accepted loss

`/resources`' pressure figures. No session keeps a pressure history, so the
figures the trace could answer from are not answerable from the log — this is
named a loss, not deferred to a fold that cannot exist. Restoring them means
recording pressure as a fact, which is a decision this one does not make.

## What this does not decide

The same change moved `Kind`'s variants into four homes. Where code lives is
`map/`, not a decision — nothing here rests on that move, and it needs no page
of its own.
