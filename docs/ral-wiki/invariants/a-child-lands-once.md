# A child lands once

**Placement is requested once; the birth applies its step, and one completion
builds the landing carried to every later reader.** `Placement::land`
(`core/src/process/launch/landing.rs`) derives every ordinary direct `Group`.
Platform code applies `Step` and registers platform resources, but builds no
`Group`. What each landing lets a collector signal is
[[invariants/a-process-group-is-its-owners-to-signal|a-process-group-is-its-owners-to-signal]].

- **Requested placement is one decision.** `Shell::group_owner` supplies stage
  membership or session permission and interactivity. `Placement::command`
  selects `Join`, `Tenant`, `Inherit` or `NewSession`; `Placement::input`
  determines terminal stdin from the request. A pipeline selects its input
  once from its anchor handoff result and stores it for every stage.
- **The direct landing follows that request.** `Inherit` shares; `NewSession`
  owns the child's group; `Join` carries its membership into `Joins`; `Tenant`
  mints a `Leader`, lends to it and returns `Owns` with the opaque terminal
  hold. Non-tenants return an empty hold. `Spawned<Group>` carries the result.
- **A terminal handoff requires typed provenance.** `Leader` certifies birth
  leading a fresh group in ral's own session, not liveness. Its constructor is
  private to `landing.rs`: only the tenant completion and `spawn_leader` mint
  it, each from the child pid its `Step::NewLeader` just grouped. It is neither
  `Clone` nor `Copy`. `PreparedTerminalLoan::lend` accepts only `&Leader`;
  a pgid, membership or session leader cannot be converted into one. The
  caller must still retain the unreaped child to pin the ID.
- **The envelope owns its placement.** Linux's `EnvelopedLaunch::spawn` calls
  `Launch::born` with `Step::NewLeader` regardless of request and hands it the
  naming of the payload's group; its landing is Linux-only `Group::Enveloped`
  on that group, or on the monitor's where the envelope names none. A
  requested `Tenant` retains readable stdin and a restoration-only hold,
  without handoff or key attribution. `Confinement::denial_pids` reads the
  payload's pids from the landed group.
- **The anchor receives a certificate, not an ordinary group.**
  `Launch::spawn_leader` returns `Spawned<Leader>` with an empty terminal hold.
  `AnchorProcess` keeps the certificate beside its child pin; the pipeline
  lends only after readiness. There is no runtime check that an ordinary
  `Group` happened to be `Owns`.
- **A launch is spent by its spawn.** Direct and enveloped spawns take their
  launch by value. `Spawned<G>` varies only its group field; terminal
  responsibility stays opaque, and nothing downstream recomputes placement.
- **One birth serves every landing and platform.** `Launch::born` creates
  the child, names an envelope's payload group, hears the launcher and lands,
  in that order; its landing receives the child's own pgid and the payload's.
  `born_direct` is the same birth with no payload to name: its landing
  receives the child's pgid alone, so a direct landing cannot be handed a
  payload group it would not address.
- **Failed birth retains the request through teardown.** `born` keeps the
  applied step, child, request and jail until the report and landing finish.
  The child is held in an `Unlanded` until it lands. Dropped unlanded, the
  guard kills the payload's group if one was named, then the child's fresh
  group exactly when `Step::leads()`, while its unreaped leader pins the ID,
  then kills/reaps the child; an exit `born` does not write still tears down. Only then does dropping the request
  restore saved state, followed by jail release. The shell settles already-started drains before reporting;
  a pipeline first rolls back and joins. No drain callback retains a reaped
  group ID. An anchor handoff refusal can restore immediately: no stage
  exists and the anchor never changes modes, so input is withheld instead.

**Why.** A group kill must address a group the child actually leads, and a
terminal loan must address a fresh leader in ral's session. Deriving the
landing once prevents platform disagreement; the certificate prevents an
arbitrary numeric pgid from receiving a loan. Neither replaces the child pin.

See [[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]],
[[internals/an-external-end-to-end|an-external-end-to-end]] and
[[internals/cancellation|cancellation]].
