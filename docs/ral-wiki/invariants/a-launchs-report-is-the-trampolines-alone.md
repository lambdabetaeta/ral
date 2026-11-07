# A launch's report is the trampoline's alone

**Nothing but the trampoline can write a launch's report, and nothing the
program runs can hold it.** The report is what a confined launch's trampoline
says of its program (`core/src/process/report.rs`; the socket is made in
`issue`, `core/src/authority/sandbox/launch.rs`). The parent acts on it: a
record is believed.

- **It is a socketpair, never a pipe.** A same-uid process can reopen a pipe's
  writer through `/proc/<pid>/fd` and forge a record; it cannot reopen a socket.
  The trampoline checks that its descriptor *is* a socket before it speaks
  (`Witness::new`).
- **The records are `refused(message)`, `unstarted(errno)`, `started` and
  `ended(status)`**, a tag byte and its payload, each composed whole and sent by
  one `write_all`, each at most once and in that order. The fixed-size records
  (`unstarted`, `started`, `ended`) are a handful of bytes, far under
  `PIPE_BUF`. A `refused` message is capped at `u16::MAX` bytes and may be
  longer; that is safe because the trampoline is the only writer, so nothing
  can interleave with it, and the reader reads exactly the length the record
  states. A cut or unknown record is an error, never an end.
- **`unstarted` is the program's own `execve` and nothing else.** On Linux
  the trampoline's child says whether it began (`die_with` for a file, signal
  defaults, mask and `die_with` for a bundled tool) on a close-on-exec pipe
  before `execve` or the tool's entry; the supervisor says `started` only
  after that word. A failed `fork` or spawn, or a child that cannot begin, is
  `refused`, and a file's spawn error is `unstarted` only when its child said
  it began. So a failure of ral's own never reads as `not-runnable`, nor as
  the program having `exited 126`.
- **The trampoline's end is typed by what it may still say.** `Witness` can say
  `refused` or `unstarted`; `started` consumes it and yields `Running`, whose
  `ended` is the only way to write `ended` (Linux), so a record out of order
  cannot be written.
- **The parent drops its copy of the writer right after the fork, on every
  path** (`Report::forked`, called by `Launch::fork` before the spawn's result
  is examined, for both `spawn` and `spawn_detached`). EOF then means every copy in the child's
  tree is closed; a parent still holding one would turn every EOF into a hang.
- **The trampoline marks the report close-on-exec after its descriptor sweep**
  (`close_inherited`, then `cloexec`), so the program never inherits it. The
  `execve` of a macOS program closes it, which the parent reads as the start;
  on Linux no program, and no descendant of one, can write a false `ended` or
  keep the reader from its EOF.
- **EOF before any record means *started* on macOS, and on Linux the envelope
  died, which is `Refused`.** The two platforms' trampolines end differently:
  macOS becomes the program, whose close-on-exec writer closes at the `execve`;
  Linux supervises, and says `started` aloud
  ([[decisions/261107_the-trampoline-is-a-supervisor|the-trampoline-is-a-supervisor]]).

**Why.** The report is the one channel on which a confined, possibly hostile
child is believed about itself: a forged `started` hides a refusal, a forged
`ended` rewrites the program's status. Authority over the channel is therefore
structural: the kind of file, the descriptor's inheritance and the parent's
copy are each fixed once, rather than checked at the reading.

See [[internals/an-external-end-to-end|an-external-end-to-end]],
[[internals/confined-launch|confined-launch]] and
[[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]].
