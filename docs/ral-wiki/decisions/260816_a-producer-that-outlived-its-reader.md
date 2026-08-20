---
status: active
---

# A producer that outlived its reader

**The pipeline exemption is about what ended the producer, not about its status
word: a non-final stage that the broken pipe itself ended keeps no failure.
Unix hears that cause as SIGPIPE. Windows, which has no such signal and no
status that means it, approximates the cause by the order the two stages ended
in, and so forgives the more of the two.**

## Decision

- `CommandFailure::from_outcome` takes a `Reader` — `Caller`, or
  `Stage { outlived }` — rather than an `is_pipeline_non_final` flag. The one
  forgiveness reads once, in one place: a `Stage` whose `outlived` is set, or
  whose producer died of SIGPIPE, is no failure.
- `outlived` is decided from kernel-recorded exit instants, never from a
  status: `ChildHandle::exited_at` answers `GetProcessTimes` on Windows and
  `None` on Unix, where SIGPIPE already names the case.
- The pipeline collector asks the question, because it is the only party
  holding both handles. It walks its stages `peekable`: the next stage *is*
  this one's reader, and the last stage's reader being the caller is what being
  final means — one traversal, no index arithmetic, and the two facts can no
  longer disagree. A stage is asked when it ended only after its producer's own
  wait returns, so a reader that outlives its producer for a moment is not
  mistaken for one that left early.
- A command run *inside* a helper stage holds no handle on the next stage, so
  it passes `Stage { outlived: false }`: SIGPIPE still speaks for it on Unix,
  and on Windows it keeps its status.

## Rejected shapes

`STATUS_PIPE_CLOSING` (NTSTATUS `0xC000_00B1`), which the tree carried as
"Windows' SIGPIPE analogue". No process exits with it: an NTSTATUS reaches an
exit status only when something terminates the process with that code, and
nothing does. What a cut-short Windows producer really exits with is whatever
its author chose — `0` for bundled `uu_yes`, `1` for a GNU-style tool that
reports a failed write, `3328` for the MSYS2 `yes.exe` a Windows runner finds
on `PATH`, that being SIGPIPE's 13 in the Cygwin runtime's own wait-status
encoding. Any constant here is a guess at a foreign convention, and three
conventions already disagree. The status cannot carry the fact, so the fact had
to come from elsewhere.

The exit-order test *on Unix* — giving the Unix path the clock the Windows path
has, so that one rule served both platforms and SIGPIPE left the forgiveness
question entirely. It is not soundly implementable. Unix records no exit
instant: `waitpid` returns no timestamp, and `pidfd`/`kqueue` exit events
deliver readiness without one, so the only witness is when a reaping thread
wakes. Measured against the two cases that matter, that witness fails both
ways. `yes | head -1`'s two deaths are one teardown cascade — the median gap
between them is microseconds, 5µs and 17µs in two runs — and the reaping stamps
invert their order on a double-digit percentage of runs, forgiving nothing and
reporting 141. `sh -c 'exit 1' | head -1` is protected only by `head`'s EOF-to-exit
latency, a median of tens of microseconds, and inverts the other way, turning a
genuine failure into a silent success. Both gap distributions straddle zero, so
no threshold and no tie-break policy separates them; a tighter implementation
shrinks the error rate without removing it, and a probabilistic verdict is not
a verdict. Windows' `GetProcessTimes` is exact where this is not, which is why
the platforms read the same fact by different means rather than by one.

## Consequences

- `yes | head` succeeds on Windows for any producer, not only for one of ral's
  own bundled tools that happens to special-case a broken pipe.
- Unix behaviour is unchanged by construction: `exited_at` is `None` there, so
  `outlived` is never set and SIGPIPE remains the whole rule.
- Windows forgiveness is broader than Unix's, and not by the sub-millisecond
  margin first recorded here. A producer that fails on its own account at *any*
  time after its reader ended is forgiven there, where Unix — the producer
  never having written to the dead wire — reports it. Measured on Linux: a
  producer that wrote nothing for a full second after its reader ended, then
  exited 4, kept its 4. The reading remains defensible where the window is the
  pipe's EOF propagation, and is an approximation beyond it.
- A producer that ignores SIGPIPE keeps its status on Unix. It takes a write
  error rather than a death, exits on its own account, and that is a failure of
  its own to report: `python … | head -1` fails where `yes | head -1` succeeds,
  and Rust's `std`, which also ignores the signal by default, puts its
  producers in the same class. This is the price of reading a cause, and it is
  paid deliberately — both alternatives are rejected above, one as a guess at a
  foreign convention and the other as unsound.

See [[design/pipelines|pipelines]], [[design/failure|failure]] and
[[map/core/io-process|core IO/process]].
