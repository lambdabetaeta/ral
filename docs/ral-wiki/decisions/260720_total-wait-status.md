---
status: active
---

# Wait status is total

> The **pid** side is superseded by
> [[decisions/260904_one-reaper-one-fold|one-reaper-one-fold]]: `waitpid_eintr`,
> `try_waitpid_eintr`, `wait_blocking_eintr`, and `classify_wait_status` go with
> `RunningChild::wait`'s last poll loop, folded into the reaper's own
> `waitid`/`WNOWAIT` scan and `blocking_reap` — one funnel now, not a pid door
> beside it. `waitpgid_eintr`/`try_waitpgid_eintr`, unreferenced since job
> control left, go too. The **pgid** side is untouched: `Pgid` still admits
> only positive identifiers, and `Pgid::signal_group` still sends by
> `kill(-pgid, …)` with no wait behind it.

**Unix wait results remain typed without ceasing to be total: rustix's
transparent `WaitStatus` carries every kernel status, while one retrying funnel
makes `EINTR` unobservable to lifecycle code.**

## Decision

- Blocking and polling are different doors. `waitpid_eintr` /
  `waitpgid_eintr` return `Result<(Pid, WaitStatus), Errno>`;
  `try_waitpid_eintr` / `try_waitpgid_eintr` alone return `Option`. The doors
  own `NOHANG`, so an impossible idle result cannot leak into a blocking
  caller.
- The doors retry `EINTR` internally. A signal delivery therefore cannot look
  like `ECHILD` and erase a live job.
- Consumers classify through `WaitStatus`'s bit-test accessors. Unclassifiable
  and continued statuses retain their raw code where the surrounding outcome
  vocabulary has no dedicated case.
- Process and process-group waits are distinct functions; callers never encode
  a process group by negating an integer pid. `Pgid` itself admits only positive
  identifiers, so the conversion to rustix's `Pid` is proved once by the type.

## Rejected shape

`nix::sys::wait::WaitStatus` is a fallible enum conversion, not a total view of
the kernel word. Its decode can reject real-time terminating signals, and an
internal assertion gives an unfamiliar status a panic path. A shell's reaper
must accept every status the kernel can produce; rustix's transparent wrapper
preserves that law.

## Consequences

- `ral/src/jobs.rs` contains neither `libc` flags nor `WIF*` decoding.
- The raw `(pid, status)` sentinel convention disappears from
  `core/src/process/signal/unix.rs`.
- Error rendering is unchanged: rustix `Errno` converts to the same OS error
  text at the existing `std::io::Result` boundary.

See [[map/core/io-process|core IO/process]].
