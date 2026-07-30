---
status: active
---

# A session disk outlives its machine by a moment, so teardown waits and start-up sweeps

**`Stopped` is the compute service's word about the machine, not the worker
process's word about its files: `vmwp` holds the session VHD open for a moment
after the machine reports stopped, so a single-shot delete at teardown loses a
race it cannot see — removal now retries over a bounded window, a disk that still
will not go is *named* on stderr rather than silently kept, and a starting
backend sweeps the session disks earlier runs left behind.** Realised in
`vm-manager/src/hcs/mod.rs`.

## Context

Teardown asked once. When the ask landed inside the worker process's exit —
which is where it usually landed, since closing the wire, waiting for `Stopped`,
and deleting the disk are three consecutive instants — Windows answered with a
sharing violation, and the disk stayed. Nothing ever read that directory
afterwards, so *stayed* meant *forever*: six orphaned session disks, about three
hundred megabytes, in `C:\ProgramData\Synod\Machine` on one real computer. The
cache is machine-wide and invisible ([[decisions/260725_windows-machine-broker|windows-machine-broker]]),
which is exactly what let the leak accumulate unnoticed.

## Decision

- **Teardown waits the worker out, but only so long.** `remove` retries every
  `REMOVE_PULSE` until `REMOVE_GRACE`; a handle that has not closed in a couple of
  seconds is one that is not closing, and teardown is on a person's own time. The
  deadline is read *after* an attempt, never before, so the disk is asked for at
  least once however late the call comes; a file already gone counts as released,
  so calling twice is as harmless as calling once.
- **A leak is heard, not raised.** `release` writes one line to stderr naming the
  file, its size in mebibytes, and what will eventually happen to it. It is not
  promoted to an `Error`, for two reasons: the same `stop` is called from `Drop`,
  where a `Result` is discarded by construction, so an error would be *quieter* in
  the common path; and a caller could not tell it apart from "the guest had to be
  stopped for", which is a claim about the user's own work and must never be
  raised over a stale file.
- **A starting backend sweeps.** `Hyperv::new` is the moment at which a backend
  is known to exist and owns no session disk of its own, so everything of that
  shape it finds belongs to a run that is over. Two guards cover the one
  exception, another synod running beside this one: its live disk is held open by
  its worker, so Windows refuses the delete; and a disk younger than `ORPHAN_AGE`
  is not even asked for, which covers the sliver between a disk being written and
  the machine being handed it.
- **The sweep cannot name what it does not recognise.** `session_disk_epoch`
  parses `synod-session-<pid>-<epoch>.vhd` and answers nothing at all otherwise,
  so the wrapped `rootfs.vhd`, the marker recording which image was wrapped, and
  the part file of a wrap in progress are structurally unreachable from it rather
  than merely excluded by a condition.
- **Age comes from the name, not the filesystem.** The epoch in a session disk's
  name was written by the process that made it; a modification time is whatever
  the filesystem last did to the file.

## Consequences

- **The cache is self-healing rather than monotone**, and the sweep says how much
  it reclaimed, so a leak that returns is visible as a recurring line rather than
  as a disk quietly filling.
- **Two housekeeping rules now hold for anything the backend leaves behind**:
  named by shape, bounded by age. The console log of
  [[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]
  was written under both, in the same cache, for the same reason.
- **The retry is testable without a hypervisor.** The fixture opens the file with
  read and write shared and *delete* denied — which is how `vmwp` holds a disk —
  so the race the single-shot delete lost to is exercised under ordinary
  `cargo test`.

## Open questions

- **`ORPHAN_AGE` is a judgement, not a measurement.** An hour is far longer than
  the window it has to tolerate and far shorter than the leak it clears, which is
  an argument for its order of magnitude and not for its value.
- **Whether the macOS backend owes the same sweep** is unexamined: `vz.rs` deletes
  its own session disks and no orphans have been seen there, which is not the same
  as knowing the race cannot happen.

## See also

[[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]
(the other thing the backend leaves in this cache, and the bounds it inherits),
[[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]] (the fixed
and dynamic VHDs, and why the session disk is one),
[[decisions/260725_windows-machine-broker|windows-machine-broker]] (whose cache it
is, and why nobody looks in it),
[[design/residency|residency]] (what stays alive after a run, and who is
answerable for it),
[[map/synod|synod]] (where the backend and its cache live).

Cite: `vm-manager/src/hcs/mod.rs` (`REMOVE_GRACE`, `REMOVE_PULSE`, `ORPHAN_AGE`,
`release`, `remove`, `mebibytes`, `sweep_orphans`, `session_disk_epoch`),
`vm-manager/src/hcs/vhd.rs` (`create_session_vhd`, `ensure_rootfs_vhd`).
