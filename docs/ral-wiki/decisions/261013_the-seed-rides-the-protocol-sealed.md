---
status: accepted
generated_at_commit: e028f5a3
generated_at_date: 2026-10-13
---

# The seed rides the protocol, sealed

**A payload the host relays but never reads crosses as bytes; its content is a
contract between the engines at its two ends, and the spawner is what makes
those ends one binary.** A sub-agent's bequest therefore rides the protocol
twice, as `FOValue::Bytes` in the parent's enquiry and as `Attach.seed:
Option<Sealed>` in the child's `Attach`, and the guest daemon, not a parent
engine, spawns every engine. Amends the sentence of
[[design/engine-protocol|engine-protocol]] that said the seed "never touches
the engine protocol"; realised in `core/src/seed.rs` (`seal`, `Sealed`),
`core/src/engine.rs` (`Engine::boot`), `exarch/src/agent/desk/agents.rs`
(`make_engine`), `exarch/src/agent/factory.rs` (`Factory`), `ral-daemon/src/init.rs`
(`make_engines`) and `synod/src/machine_factory.rs`.

## Context

Inside the guest only an engine could start an engine, so a parent engine did
the host's job: it bound a guest port, minted a token, kept a listener thread,
re-exec'd `--engine` with the seed on an inherited fd, wrote an ack byte, and
kept a table of children it swept but never killed, so a dead parent's
children were orphans the daemon buried. Each of those existed because the
guest had no process that made engines on request, though the daemon already
spawns the root onto a connection on fd 3 and `engine::spawn` is a function of
any connected descriptor. The one-fork construction
(`Shell::child_of`, `Runtime`/`Image`/`Terms`, `Bequest`) made the seed a pure
codec; what remained was who spawns.

## Decision

**The opacity law.** The first-order law governs *positions the host reads*.
The seal is read by no host: in the enquiry it is `FOValue::Bytes`, a
first-order datum the desk relays untouched, and in `Attach` it is one closed
envelope field gated by `PROTOCOL_VERSION` like every other. The two parties
that do interpret the bytes are two instances of one binary, both spawned by
the factory from one path (`ral.engine` on a read-only rootfs); their agreement
is structural, not versioned. `Sealed::unseal` is `pub(crate)` to core, so the
host cannot read it.

**Every engine is born from an `Attach`.** `seed: None` runs the root recipe;
`Some` unseals against the engine's own `Runtime` and constructs with
`Shell::child_of`. `Engine::boot(installers, &attach)` is the one door.

**The host makes engines; the daemon is the factory.** The wire arm of the fork
builtin does no I/O: `bequeath`, `seal`, name the bytes in the enquiry as
`` `sealed <bytes> ``. The desk dials its `Factory` (`synod`'s is
`MachineFactory` over `Machine::connect_guest`), attaches the child as the
parent was attached plus the seal, waits for `Attached`, and only then answers.
The daemon listens on `FACTORY_PORT` for the VM's life and spawns one engine
per accepted connection, exactly as it spawns the root. The root keeps dialling
out (its dial is the boot's readiness signal); children are dialled in, because
the host already holds the spawn the connection is for.

**Admission is the peer's CID.** The daemon accepts only `VMADDR_CID_HOST`
(`is_host`) and drops anything else. A guest process cannot present the host's
CID: the source CID is written by the guest kernel's transport. Losing the
token loses nothing.

**The scratch is the host's, on every seat.** Every `Attach` names
`$EXARCH_SCRATCH`; `engine_root` refuses one that does not; no engine mints a
scratch, so no lock crosses a process boundary and nothing must live as long as
an engine.

**Who ends what.** A child ends when the host drops its connection (EOF on
fd 3); `halt` signals every engine's process group.

## Why no stamp

A build-id stamp on the seal would guard against a mismatch the spawner makes
impossible, and the one agreement that can drift, host against guest media, is
already gated by `PROTOCOL_VERSION` at `Attach`, checked before any seed is
decoded. The boot contract moves to 3
([[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]]) for
the factory port.

## What this deletes

The hatch: `seed/hatch.rs` (`listen_for_hatch`, `hatch_over`, `seed_from_env`,
`HATCHED` and its sweep), the token, `HATCH_ACK`, the seed fd
(`RAL_ENGINE_SEED_FD`), the table of unreaped children, the guest port builtin,
`Birth`, `Dial`/`MachineDial`, and the orphan question: no engine has children
it must outlive.
