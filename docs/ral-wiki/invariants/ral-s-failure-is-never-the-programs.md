# Ral's failure is never the program's

**`Unstarted::Program` is the program's own failure to become a process and
nothing else; everything that is not its fault is `Refused`, and a stop is
`Stopped`.** The type has three cases (`core/src/process/launch.rs`), and each
case is a claim about blame that the run's error then repeats to the user.

- **`Program(io::Error)`** is the `execve`'s own error, whether the parent's
  (an unconfined launch) or the trampoline's `unstarted(errno)` (a confined
  one). It is worded as the process failure the probe would have reported:
  `not-found`, `not-runnable`, with the image deciding what `ENOENT` means.
- **`Refused`** is never the program's fault. It covers a launcher or envelope
  that fails to exec, a payload process group that cannot be named, the
  trampoline's `refused`, an envelope that died before reporting, and a step of
  ral's own that failed before the `execve` (placement, signal reset, limits,
  the guest jail, stdio, `detach`'s fork), confined or not (`unprepared`:
  "ral could not prepare the command's process"). Only an `Exec` failure
  depends on confinement.
- **On Windows only `CreateProcessW`'s own error is `Program`**; a `.bat` or
  `.cmd`, a NUL in an argument and handle admission are `Refused`.
- **`Stopped`** is a cancellation while waiting for the trampoline's first word
  (Unix only); it is the run's stop, not a failure.

**Why.** An error blames a subject: `prog: permission denied` accuses `prog`.
A launch that fails in ral's own step, a join into a process group that is gone
or a working directory deleted under it, would accuse the one program that was
never started, and a host without `bwrap` would read as a grant that denies
everything. The cases are therefore fixed by *where* the failure arose, not by
the `errno` it carries: the same `EACCES` is the program's at its `execve` and
ral's at its `setpgid`. No `io::Error::other(String)` crosses this boundary;
text that is ral's own is a `Refusal`.

How the parent tells the two apart for an unconfined launch is
[[invariants/no-mark-ral-s-fault|no-mark-ral-s-fault]]. The one conversion of
`Unstarted` into the run's error is `unstarted_error`
(`core/src/shell/exec/command/process.rs`); see
[[internals/an-external-end-to-end|an-external-end-to-end]] and
[[design/exec|exec]].
