# No mark, ral's fault

**A spawn error is the program's only if the child had already made its mark;
without the mark, one of ral's own steps failed.** The last pre-exec step of
every Unix spawn writes one byte on a close-on-exec socket
(`spawn_with_pgid_after`, `core/src/process/group/unix.rs`). The `execve` that
follows can then fail only as the program's own failure, and `std`'s errno
already says which.

- **The mark is one byte on a close-on-exec socketpair.** The parent reads its
  end without waiting (`attributed`, `recv` with `DONTWAIT`): the mark precedes
  the `errno` that `std` has already read, so a byte absent is a byte never
  written. With the mark, the error is `SpawnError::Exec`, hence
  `Unstarted::Program`. Without it, `SpawnError::Prepare`: the fork, the
  placement, a `pre_exec` hook of any caller, or `std`'s own steps before them
  (a NUL in an argument).
- **The closure owns the socket.** The pre-exec closure is `move`, so the fd it
  writes is valid for as long as the `Command` lives, whenever and however
  often it is run. Nothing it writes to can have been closed or reused behind it.
- **The mark is the last step.** Hooks run in registration order, so a caller's
  own (rlimits, the guest jail, `2>&1`) precede the placement, the signal reset
  and the caller's `after` hook, which precede the mark. A step added later
  than the mark would be blamed on the program.
- **On `detach`, the grandchild writes the mark and the intermediate never
  does.** The hook forks; the intermediate writes the grandchild's pid and
  `_exit`s inside the hook, short of the mark; the grandchild goes on to make
  it. `Ok` therefore proves the *grandchild* reached its `execve`, and a failure
  before it is ral's.

**Why.** The alternative reads an `errno` for whose it is, and the same errno
means both: `EPERM` from `setpgid` into a group nobody leads is not
`permission denied` of a program. Blame by `errno` mislabels ral's failures as
the program's; blame by position cannot. The rule fixes the position of the
mark, and nothing else.

See [[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]]
and [[internals/an-external-end-to-end|an-external-end-to-end]].
