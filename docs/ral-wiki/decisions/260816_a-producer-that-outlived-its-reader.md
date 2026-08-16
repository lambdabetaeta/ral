---
status: active
---

# A producer that outlived its reader

**The pipeline exemption is a fact about the two stages, not about the
producer's status word: a non-final stage still running when the stage reading
it ended keeps no failure, whatever it exited with. Unix says that with
SIGPIPE; Windows, which has no such signal and no status that means it, says it
with the order the two ended in.**

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

## Rejected shape

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

## Consequences

- `yes | head` succeeds on Windows for any producer, not only for one of ral's
  own bundled tools that happens to special-case a broken pipe.
- Unix behaviour is unchanged by construction: `exited_at` is `None` there, so
  `outlived` is never set and SIGPIPE remains the whole rule.
- Windows forgiveness is very slightly broader than Unix's. A producer that
  fails on its own account in the sub-millisecond window *after* its reader
  ended is forgiven, where Unix — the producer never having written again —
  would report it. The window is the pipe's EOF propagation, and the reading is
  defensible in it: nobody was left to receive the output.

See [[design/pipelines|pipelines]], [[design/failure|failure]] and
[[map/core/io-process|core IO/process]].
