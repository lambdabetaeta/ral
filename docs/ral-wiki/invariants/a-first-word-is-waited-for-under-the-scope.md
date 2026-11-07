# A first word is waited for under the scope

**The parent's wait for the trampoline's first word is a cancellable wait: it
reads the launch's own scope between slices of `POLL_SLICE`, and a stop ends
it.** `Testimony::first` (`core/src/process/report.rs`) is that wait. The scope
is the one the launch's `Confined` holds, set when it was confined
(`Launch::confine`).

- **On a stop, the child is discarded and the result is `Stopped`.** The child
  is killed and reaped (`discarded`, `core/src/process/launch.rs`); the launch
  never became a program. `Stopped` reaches the run as its stop, with the cause
  and owner of the scope that struck.
- **`detach` leaves its survivor.** A detached launch waits for the same word
  under the same scope, but its child is nobody's to kill: a stop ends the wait
  and the survivor lives on.
- **The wait cannot be a blocking read.** On Linux a program can stop its
  supervisor before it says `started`, leaving the report open and silent; a
  blocked read would then never run the scope's poll, and neither Ctrl-C nor a
  deadline could end it.

**Why.** Every wait in ral is one a cause can end
([[internals/cancellation|cancellation]]). The first word is the only wait in
the start, and an uncancellable one would be the one place a stopped program
hangs the shell.

See [[internals/an-external-end-to-end|an-external-end-to-end]].
