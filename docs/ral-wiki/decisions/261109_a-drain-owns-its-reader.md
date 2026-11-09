---
status: active
verified_at_commit: 6e2995f8
verified_at_date: 2026-11-09
anchors: [Connection, PendingDrains, PendingDrains::start, Drains, ConnectedCommand, ConnectedCommand::spawn, Started::register, Launch::dup_stdout_to_stderr]
---

# A drain owns its reader

**Preparing an output drain constructs its pipe and retains the reader with
its destination.** Starting the drain needs no lookup in a child's optional
standard-stream fields.

- **Connection is the boundary.** Shell sources and sinks describe byte
  destinations; a connection prepares OS endpoints; the process launcher
  installs them. Existing streams are borrowed, owned downstream writers move,
  and only a destination needed by a drain thread is cloned.
- **Topology stays with the pipeline.** Its routes distinguish a parent
  boundary from an interior edge. A direct external consumes the writer; a
  thread stage receives a pipe sink carrying its own cancellation identity.
  Command execution has no second representation of that route.
- **Drains start before the child.** A configured launch and its pending
  drains are held together until spawn, which starts the drains first.
  Connection failure releases prepared resources. A drain thread that cannot
  start fails the launch with no child to tear down: dropping the launch
  closes every writer, so the drains already started end at EOF having carried
  nothing; spawn failure ends them the same way. Registration cannot fail.
- **Equal destinations share an endpoint.** Both process backends implement
  stderr aliasing the final stdout endpoint, so a merged capture has one reader
  and one drain, including on Windows.
- **Settlement retains its order.** The terminal returns before the jail is
  finished and drains are joined. Cancellation kills before joining; forced
  teardown may detach drains. Drain destruction never introduces a wait.

The distinction is ownership, not a new stream trait or an input/output
symmetry: inputs already have descriptors; some output destinations require a
parent-side computation.

## Where

- `core/src/io/child.rs`: connection, owned pending readers, running drains.
- `core/src/shell/exec/command.rs`: the connected-command state.
- `core/src/shell/exec/pipeline/{launch,thread,collect}.rs`: route consumption,
  registration, and settlement.
- `core/src/process/launch.rs`: descriptor installation and stderr aliasing.

See [[map/core/io-process|io-process]], [[map/core/runtime|runtime]],
[[internals/output-capture-and-detachment|output-capture-and-detachment]], and
[[invariants/the-terminal-returns-before-the-pumps|the-terminal-returns-before-the-pumps]].
