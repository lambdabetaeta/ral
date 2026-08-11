---
status: active
---

# A survivor is confined by the frame that bore it

[[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]
left one question open and named the shape of its answer: *"`detach` under a
sandbox is either impossible or requires escaping the envelope — which is a
capability question, not a lifetime one, and probably wants a
[[design/grant|grant]] authority of its own."* This is that ADR. The answer is
that it was never impossible; the envelope needed one flag dropped, and the
authority needed a name.

## The obstacle was a flag we chose, not a property of confinement

`core/src/sandbox/linux.rs` passed bwrap `--die-with-parent` on every launch.
That is `prctl(PR_SET_PDEATHSIG, SIGKILL)` over the whole envelope, and it is
there for a good reason: a confinement orphaned by a crash should not outlive
the session that authorised it.

Against a double fork it is not merely unhelpful — it is *indeterminate*. The
survivor's parent is the intermediate, which `_exit(0)`s as soon as it has
written the pid back. Whether the flag ever fires depends on which of bwrap's
`prctl` and that `_exit` the scheduler runs first: lose the race and the whole
sandbox tree is `SIGKILL`ed moments after birth; win it and the flag is dead,
because pdeathsig set after the parent is gone never fires. Either way the
receipt describes something that may not exist. The old gate read this as "a
sandbox kills its envelope, so a survivor under one is a promise the OS
breaks", which took a race we had introduced for a property of sandboxes.

The two facts that dissolve it:

- **Nothing else about the envelope is tied to us.** Mount namespace, network
  namespace and seccomp filter are entered at `execve` and belong to the
  process, not to a relationship with a parent. Dropping the tie changes the
  envelope's *lifetime* and nothing about its *contents* —
  `only_the_parent_death_tie_distinguishes_a_surrendered_launch` asserts
  exactly that, by diffing the two argvs.
- **macOS never had the problem.** Confinement there is `sandbox_init` applied
  in-process by a re-exec'd ral, which then `execve`s the target in place
  (`sandbox/launch.rs`, `serve_sandbox_exec`). One pid, no supervisor, no
  parent coupling, and Seatbelt is inherited and irrevocable. The uniform gate
  was denying macOS a thing macOS could always do.

So `Ownership::{Kept, Surrendered}` is threaded from the call to the bwrap
argv, and decides `--die-with-parent` alone. A kept child keeps the tie — the
crash-orphan reason still holds for everything we wait on.

## Confinement frozen at birth is stronger than the ban it replaces

The old rule permitted survivors only where *no* sandbox engaged, so every
detached process in existence was born unconfined, under `--base dangerous`.
The new rule births them under whatever projection the frame carried, and that
projection is then permanent: no later frame can widen it, because no later
frame — and no later session — can name the process at all. A `grant` narrow
enough to be worth writing is narrow around the survivor too, for as long as
it runs. Trading "unconfined survivors only" for "survivors confined for life"
is not a loosening.

## The authority is a dimension, because attenuation is what a grant does

`detach: Option<bool>` joins `net` on the capability lattice, meets like every
other axis, and is folded at the call by `GrantStack::permits_detach`.

**Silence permits.** An ordinary `grant fs: […] { … }` says nothing about
survivors, exactly as it says nothing about the network. The alternative — a
positive authority a frame must opt into, the shape a lease takes — was
rejected: it would be the only dimension in the system where absence of an
opinion denied rather than inherited, and every base that wanted survivors
would have to name a key to get back what it already had. `detach: false`
withholds, and no inner frame grants it back.

The fold lives on the stack rather than in `SandboxProjection` because nothing
about it reaches the OS profile. It gates a verb; the projection describes a
confinement. Putting it in the projection would have meant serialising a
question the sandbox backends have no use for.

## Absence and refusal stay different axes

[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] wants a host that
cannot do a thing to *lack* the name. That still holds, and is untouched: the
`ral` hosts and the headless runner install no `detach`, Windows has no double
fork, and naming it there is an unknown-command error.

What changed is which question absence answers. It is now purely a host's
answer — "is this verb meaningful on this host at all" — and no longer smuggles
in a capability judgement made once at boot. A frame's answer is a *refusal*,
which is what every other frame-scoped denial already is: an `fs` deny is not
absence of the write, it is a no. `engages_sandbox` existed only to make that
boot-time judgement and is deleted.

The refusal precedes admission against the birth budget, matching the rule the
first ADR set for handled names: nothing born, nothing spent. The budget itself
is unchanged — one monotone session-wide counter. A per-frame allowance was
considered and rejected: births inside a frame are unobservable after it exits,
so a frame-scoped counter would have no honest release point and would be a
second monotone counter rather than a seat.

## What is still not tested

The old ADR listed behaviour under `--die-with-parent` as untestable because a
`detach` under confinement was refused. It is now reachable, and what replaced
that gap is a split: whether a frame *permits* the verb is checked through the
public door (`core/tests/detach.rs`), while what the survivor is confined *to*
is checked where the argv is built, not by birthing a process and interrogating
a namespace the test cannot enter. On macOS the permitted-birth test is a real
birth under a real Seatbelt projection, since any `fs:` attenuation raises one.

One thing is asserted from reading rather than from running: whether bwrap
execs the target in place or keeps a supervisor process decides whether the
receipt's pid names the program or the envelope. Under a Linux projection the
receipt may name bwrap. That is a *documentation* question about what `pid`
denotes, not a confinement question, and it wants one run on Linux to settle.

## See also

[[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]
(the verb, and the open question this closes),
[[design/grant|grant]] (the lattice this adds a dimension to),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (absence versus
veto, and why both survive here), and `docs/SPEC.md` §12.6, §12.9, §11.5.
