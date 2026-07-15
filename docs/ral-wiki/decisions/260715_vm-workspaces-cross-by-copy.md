---
status: proposed
---

# VM workspaces cross by copy, never by mount

**An exarch VM receives one private workspace copy over a bounded bulk channel;
no host directory is mounted, and no guest change reaches the granted tree
without validation, a witness check, and explicit publication.** The VM is a
place to execute hostile code, not a second view of the host filesystem.

## Decision

The boundary has two virtual-socket planes:

- the existing ral seam carries turns, enquiries, surface values, and control;
- a daemon-owned bulk plane carries versioned, content-addressed workspace
  snapshots in bounded chunks. Workspace bytes never ride `FOValue` JSON.

The host provisions a private immutable tree and object history, streams its
baseline into guest ext4, and receives a delta only at an engine-issued
quiescence barrier. The receiver admits directories, regular files, symlink
text, and one executable bit. It rejects absolute and parent paths, special or
hard-linked files, case collisions, invalid names, and every configured size or
count excess. A complete delta materialises a new immutable tree; one pointer
advance publishes it to the private session, while a mismatch leaves the old
tree intact.

The granted folder changes only through an explicit host publication step.
There is no live-apply mode in the secure design.

## Authority

The VM is only the outer ceiling. [[design/two-enforcers|Ral's two enforcers]]
remain inside it:

- the in-process gate judges ral-owned effects and argv;
- each external spawn receives an OS jail projected from the effective
  [[design/grant|capabilities]].

Thus a fork still receives `parent ⊓ base`, and a narrower turn cannot recover
the whole `/work` mount merely by spawning a process.

Host git is an authority desk, not a guest executable. Checkpoints use isolated
plumbing with no hooks, filters, attributes-driven programs, external diffs,
signing, pagers, credential helpers, or model-shaped argv. The guest receives
typed readings, never a gitdir.

## Network

The VM has no NIC. A net-enabled child reaches a loopback CONNECT stub whose
only egress is a dedicated virtual-socket port to the host consent proxy; a
`net: false` jail cannot reach the stub. DNS, destination checks, consent, and
provider credentials remain host-owned.

## Consequences

- Build I/O stays on guest ext4; the host filesystem is absent by topology.
- A separate daemon may own init, supervision, and bulk transfer, while the
  existing ral/exarch multicall binary remains the engine.
- Seam Phase C must provide multi-session multiplexing, per-session
  cancellation, engine-side durability, bounded decoding, and a strict frame
  automaton before the VM backend is coherent.
- A guest crash loses unacknowledged guest state; the host retains the last
  acknowledged private checkpoint.
- Private dependency credentials, PTY forwarding, server ingress, lazy huge
  trees, warm pools, and weaker live application remain separate decisions.

## Rejected

- **A writable host directory shared into the guest.** It exposes the host
  filesystem implementation and lets host editors or watchers interpret guest
  changes before review.
- **Turn deltas inside the ral value channel.** Bulk bytes have different
  framing, backpressure, retry, and allocation laws from first-order control
  values.
- **Fresh UIDs as the whole guest sandbox.** Identity isolates processes from
  one another; it does not express ral's path, read/write, exec, or network
  lattice.

See also [[decisions/260706_enquiry-channel|enquiry-channel]],
[[design/exarch-architecture|exarch-architecture]],
[[design/agents|agents]], and [[design/residency|residency]].
