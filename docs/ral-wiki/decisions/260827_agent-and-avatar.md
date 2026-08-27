---
status: accepted
---

# An agent is the `Arc`

**An agent existed three times, and the last two bugs fixed were the copies
disagreeing; the fix is not a fourth copy but two types split along who may
touch what.** [`Agent`] holds the public half — identity, the two cancel
doors, mailbox, provider, immutable config, and a small single-writer
`Status` — behind an `Arc` the fleet shares. [`Avatar`] holds the private
half — the log, the seat, the inbox, everything only the attend thread
touches — plus the `Arc<Agent>` it embodies. An agent is *live* while its
avatar holds that `Arc`; nothing deregisters.

## The diagnosis

`Agent` (`agent.rs`) owned the cancel token, seat, provider, log cell,
parent id, fuel, caps, `returns`, `interactive`, `search`. `registry::Entry`
(`fleet/registry.rs`) held *copies* of the token, reach, mailbox, provider,
name, log dir, and parent — plus the state that lived only there:
`generation`, `consumer`, `rest`, `reply`, `last_exchange`. Entries sat in
one `HashMap<AgentId, Entry>` under one mutex. `HostServices` (`fleet/desk.rs`)
snapshotted twenty-odd fields off `Agent` at desk install, because a handler
could not hold `&Agent`.

Everything reached an agent through the map by id. `AgentRegistry` had 34
methods across roughly 2,000 lines; each began with a lock and a lookup that
could fail, so each grew an `Unknown…` arm. Scope checks walked parent ids
over a flat map. The sync between `Agent` and `Entry` was by convention —
`register_self`, re-register-in-place, "leased iff parented" — and the last
two bugs fixed were the two copies disagreeing.

Looking at *which* fields got copied is what found the line the type itself
did not draw: `Entry` and `HostServices` between them wanted identity, the
two cancel doors, mailbox, provider handle, immutable config, and the small
mutable status. Neither ever wanted `seat`, `nudges`, `last_input`, the warn
latches, or the epochs — the fields the attend loop mutates through
`&mut self`. An agent has a **public half**, which anyone may touch and
therefore existed twice, and a **private half**, which only its own thread
touches and therefore should exist once. The bug was the doubled public
half; the fix is not to share the private one either.

## The decision

Two types, along that line ([`agent.rs`](../../../exarch/src/agent.rs) doc
comment):

- **`Agent` is the `Arc`.** Identity and immutable config, fixed at
  construction, except `status` (the avatar's own register) and `children`
  (the spawn site pushes onto it). Held by its avatar, its parent, and the
  fleet's by-name/by-id doors.
- **`Avatar` is the embodiment.** Everything only the attend thread touches,
  plus the `Arc<Agent>` — so every method the loop calls is plain
  `&mut self`, with no lock and no lookup between a call and the field it
  reads.
- **Liveness is the avatar holding the `Arc`.** Nothing deregisters; the
  parent's `children` and the fleet's `roots`/`names` hold `Weak`, and every
  walk prunes what fails to upgrade. `Avatar`'s `Drop` is the whole of what
  `deregister` used to do explicitly: it clears the avatar's own armed
  schedules and records the session-ended bookend, and the last strong
  reference going with it is what a parent's or the fleet's next walk prunes
  away.
- **Status has one writer.** Of `Entry`'s mutable fields, `generation`,
  `rest`, and `reply` are written by the avatar alone and read by everyone
  else — a process publishing its own status. `Agent::status` is a `Mutex`
  that buys one atomic snapshot per read and computes nothing under the
  lock; nothing external ever writes it.
- **The exchange clock is the inbox's.** `last_exchange` was never agent
  state — it is "when did a human or parent last push into this mailbox",
  a fact the inbox itself witnesses. It moved to `Mailbox`, stamped by
  `Mailbox::steer` (a human line) and by `Agent::message` (a parent's
  marked note) before the item is pushed, so a park verdict woken by that
  push already reads engaged. `renew` as a method is gone with it —
  `Mailbox::steer` is one door, not renew-then-drop-then-push.
- **Up is strong, down is weak.** A child holds `Arc<Agent>` to its parent
  (`Agent::parent`), so `deposit_reply` and the scope climb never dangle; a
  parent whose avatar has gone is still a reachable `Agent` with a
  terminated token, not an absence. `children` holds `Weak`, so there is no
  cycle to reason about and a walk prunes what settled. **Nothing holds an
  `Arc<Agent>` to a descendant** — the invariant every reaper closure and
  every upward result respects by carrying `Weak`, not `Arc`.
- **The lock rule.** Two per-agent mutexes, `status` and `children`; hold at
  most one at a time. `children` is locked only to push or to snapshot,
  never across a walk. `status` is a single-writer register. `parent` is an
  `Arc`, read lock-free. The inbox → agent order stands unchanged (a park
  verdict reads `agent.rest()` under the consumer's own inbox mutex), and
  nothing now needs the other direction: `steer` touches only the inbox.

`Fleet` (`fleet.rs`) shrinks to `{ names, roots, lease }`: two `Weak`
indices — the by-name door a spawn claims identity at, the by-id door a
frontend command resolves through a pruning walk from `roots` — plus the
idle-lease duration every reporting child is armed with at `Fleet::enrol`.
It is not the tree; the tree is `Agent::parent`/`Agent::children`, and every
walk — the roster, the cancel cascade, the scope check — runs there. A
`/branch` is a root exactly as the trunk is: parent-less, pushed onto
`roots`, reporting to nobody.

`HostServices` (`fleet/desk.rs`) is `Arc<Agent>` plus what is per *call*:
`nursery`, `reply: ReplyCell`, `emit`, `acts`, `principal`, `generation`,
`cwd`, `kind: SeatKind`, plus the avatar's `LogCell`.  `schedules` and `pins`
are public agent state and live on `Agent`. `generation` and `cwd` stay snapshots on purpose —
a desk older than its own `/clear` must refuse to spawn, and the model
starts a spawned child where it was, not where a live `cd` has since moved
to.

Trunk cancel publication — `cancel::publish`, the OS-signal path a
sub-agent never needs because its token is reached through the tree, not a
slot — moved to the sites that actually launch a process trunk:
`headless::run`, `headless::converse_settled`, and `tui::tui_loop::run`.

## What this deleted

`registry::Entry`; `HostServices` as a field-by-field copy; `register_self`,
`register_self_named`, and re-register-in-place; `deregister`; `renew` and
`renew_entry`, and the renew-drop-push choreography inside `steer`/`message`;
the `Unknown*` refusal variants and their match arms; `is_descendant_of` as
a named predicate (folded into the `descendant` climb); the "absent reads
0" convention for a stale or unknown id; roughly 20 of `AgentRegistry`'s 34
methods, and the lock-and-lookup prologue every one of the rest used to
open with.

## Risks

- **A strong edge pointing down** would be the one way to break this: if
  anything ever holds `Arc<Agent>` to a descendant, the weak-child invariant
  stops meaning what it says. The rule is stated at the top of `agent.rs`
  for exactly this reason — reaper closures and upward results must carry
  `Weak`.
- **Reading live where a snapshot was load-bearing.** `HostServices::generation`
  and `::cwd` are deliberate snapshots, not staleness bugs — `fleet/desk.rs`
  states as much on the field itself ("a desk older than its own `/clear`
  refuses to spawn"); anywhere else a future change is tempted to read
  `self.agent.…` live instead of a captured field needs the same scrutiny.
- **Tests keyed by literal ids** — `spawnable_desk`, the `fleet.rs` tests,
  `reply_cancels_live_descendants` — became two-agent trees built with
  `agent/testkit.rs`'s `test_agent` and no thread behind them; they read shorter,
  but a future test reaching for a bare id instead of a held `Arc<Agent>`
  is reaching for a pattern this change retired.
- **The wire.** Nothing on it changed; only how an id is resolved on
  arrival — a pruning walk from `Fleet::roots` rather than a map lookup.

## Supersession

[[decisions/260826_reply-parks|reply-parks]] is **superseded only in the
mechanism it named, not in the decision it made**: reply still deposits and
parks rather than ending the agent, the parent still fetches with
`` agents `read <name> ``, and the roster's `state`/`idle-s` derivation is
unchanged. What no longer holds is the sentence "a returning agent's `reply`
deposits its value on the agent's own registry entry" — there is no
registry entry; the value lives on `Agent::status.reply`, written by the
avatar and read by anyone still holding the `Arc`. A pointer is added at
that entry's top.

## See also

[[design/agents|agents]] and [[map/exarch/agent|agent]] (rewritten against
this shape), [[design/residency|residency]] (the resident ledger this
tree is a chapter of), [[decisions/260825_the-host-dials-in|the-host-dials-in]]
and [[decisions/260825_the-wire-carries-the-value|the-wire-carries-the-value]]
(landed the same week, on the enquiry side of the same fleet).
