# A first word is waited for under the scope

**The parent's wait for the trampoline's first word is a cancellable wait: it
reads the start's scope between slices of `POLL_SLICE`, and a stop ends
it.** `Testimony::first` (`core/src/process/report.rs`) is that wait. The scope
is the one the start hands the spawn (`Launch::spawn`, `spawn_detached`), the
run's own; a launch is built under none.

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

**Why.** Every wait on a program is one a cause can end
([[internals/cancellation|cancellation]]). On Unix the first word is the only
wait in the start that a program can prolong, and an uncancellable one would be
the one place a stopped program hangs the shell. (On Linux the start also reads
bwrap's `--info-fd` record to EOF, `payload_pgid`; that read is not cancellable,
but it ends when the trampoline sweeps its descriptors, before it runs anything
the program owns, or when the envelope dies.) Windows' start has no first word; its waits are the
stamps, which poll the same scope
([[internals/confined-launch|confined-launch]]). The claim is about waits on
a program. The waits no cause ends are on what ral has itself already ended:
the intermediate of a `detach`, which has exited (`survivor_pid`), and an
anchor's reporter thread, which ends when the anchor's pipe closes
(`AnchorProcess::end`).

See [[internals/an-external-end-to-end|an-external-end-to-end]].
