# Agents: the uniform node, the tree, and the parent-less trunk

**An exarch run is a *fleet* of structurally identical *agents* arranged in a
tree, driven by the single shared `attend` loop; the only thing that distinguishes
one agent from another is its *position* — whether it has a parent.** A sub-agent
is not a different machine: it is an `Agent` ([[map/exarch/agent|agent]]) forked
from a value-snapshot of its parent's shell, under its parent's capability
stack plus one layer, with a strong `Arc<Agent>` parent edge. The thin `Fleet` holds what every node
shares — the by-name door, the idle lease, the one event bus, and
the transport engine ([[decisions/260827_agent-and-avatar|agent-and-avatar]]).

## One predicate, fixed at construction

There is no `is_root`. Whether an agent returns a value or converses with a human
is a **construction-fixed `returns` bit** on the `Agent` — `true` for a `fork`ed
sub-agent, `false` for a `/branch` child, `!interactive` at the trunk. One bit is
the single source of truth for every role reader: `returns()`, parking's
conversing predicate, the desk's `reply` refusal
([[map/exarch/builtins|builtins]]), and the per-agent builtin index resolved from
the same bit at `Avatar::assemble` — so reply availability, parking, and the
advertised vocabulary cannot disagree
([[decisions/260705_branch-minimal|branch-minimal]]). **Fuel is a separate
construction-fixed depth budget:** an agent's spawn surface is present exactly
when `fuel > 0`; it does not decide whether the agent returns. Position still
does two jobs — it fixes the tree edge (`parent`) and the signal path (only
the trunk's session is minted facing the ambient causes,
[[decisions/260726_cancel-is-a-join|cancel-is-a-join]];
[[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]) — but it does
not decide who returns.

- **A returning agent holds `reply`.** A returning node at any depth, *and* a headless trunk
  (`parent = None`, `interactive = false`) seeded once to produce one result, both
  advertise it and terminate at quiescence. This is
  [[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]'s
  reply gate, read off the construction-fixed bit rather than off
  `is_root && interactive`.
- **A conversing agent has `reply` withheld at construction** and parks for a human
  instead of returning. Its `reply` call is refused at the desk, and the verb is
  dropped from its builtin index, both keyed on the same bit. The interactive
  trunk is one such agent — parent-less, its writer ever-present — but not the
  *only* one: a **branch** is interactive, `reply`-withheld, and — like the
  trunk — parent-less, a root of its own tree rather than a descendant
  ([[decisions/260705_branch-minimal|branch-minimal]]). "Parent-less" and
  "converses" are not the same set, which is exactly why the property is a bit
  fixed at construction rather than a predicate on position.

"Returns a value" and "does not park for a human" remain the *same fact*, read in
one place ([[map/exarch/agent|agent]]). Parking is **computed, not stored** — a
`ParkMode` (`Held` / `Engaged` / `HeldByChildren` / `UntilCancelled` / `Quiesce`)
derived on every wake: a conversing agent parks `Held` while its own cancel
token is unterminated, a returning agent a human has exchanged a message
with parks `Engaged` bounded by the fleet's idle lease, and everyone else
quiesces.

## Prompt obligations follow construction

The prompt is an *agent-invariant base* plus one per-agent resolution step. The
same construction facts that govern the desk govern what the model is taught:

- **`returns` gates `# Agent`.** Returning agents receive the deliberate-return
  contract; conversing agents do not.
- **`interactive` gates `Surfacing`.** Human-facing guidance remains in the
  shared base, so a returning interactive child can receive both it and
  `# Agent`.
- **`fuel > 0` gates `# Agents`.** A leaf receives neither spawn
  guidance nor the spawn family in its builtin index.
- **The builtin index has three bits** — `returns`, `allow_schedule`, and
  `spawns` (`fuel > 0`) — cached once from the booted shell. The unresolved base
  stays on the node, and roots, identity forks, and wire children resolve their
  own prompt before the log bookend records its byte length.

## Uniform spawning: bounded by spawn fuel

Spawning is **available to every agent with fuel left**, so the spawn tree is
not capped at one level. The effective `spawns` bit is `fuel > 0`; when it is
false, the prompt index omits `exarch-agents` as a dead family, while the desk remains
the runtime authority. Depth-N works
structurally — a child enrols in the fleet by name and joins its parent's
`children` and `fork` snapshots the parent's shell by value at any depth —
but each `fork` hands the child one
less unit of `fuel` than the parent holds (the parent's own `fuel` is untouched,
so fan-out itself is unbounded), and a `fuel == 0` agent's spawn call is refused
at the desk with the exhaustion text: a delegation chain terminates by refusal
a fixed number of generations down rather than recursing forever
(superseding the depth-1 cap of
[[decisions/260617_async-agent-tool|async-agent-tool]];
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]], bounding the depth
that decision left open).

The `` exarch-agents `start `` tag is **launch-only and always asynchronous**
([[decisions/260617_async-agent-tool|async-agent-tool]]). Its argument is a
single closed record
`[prompt: …, name: …, type: …, grant: …, search: …, provider: …, model: …]` —
a record literal, so a missing or misspelled field is a *static* error naming
it, while the `type` (`` `amnemon ``/`` `mnemon ``), `grant`, `provider` and
`model` tags are
checked at the runtime door that enumerates their legal labels
([[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]).
One call:

- **`fork`s a child `Agent`** through `Shell::fork_scrubbed`
  ([[map/core/shell-state|the flow matrix]]). The child snapshots the
  **serialisable fragment** of its parent's lexical scope — every binding,
  each handle in it replaced by an `` `opaque `` placeholder — with the
  dynamic context (cwd, env, grants, handlers) and the installed builtin table;
  sets `parent` to the spawning agent's id, takes the spawn's `name` as its
  identity, and starts fresh in everything else — its own inbox, a fresh cancel
  token, an owned provider handle seeded from the parent's current model, no
  terminal authority. This is a **value snapshot**: the child's `cd`, env, and
  new bindings die with it; there is no flow-back, and the parent receives a
  string, not the child's bindings. The isolation mirrors a
  [[design/pipelines|byte-pipeline stage]]'s subshell — see
  [[#Wire-seat children: the same snapshot, a different wire|below]] for why
  the promise is the serialisable fragment rather than the whole scope;
- **runs it on a detached thread** through the same `attend` loop, answering
  at once with the roster afterwards — the child's row carrying `name`,
  `elapsed-s`, and `log-dir` — a ral record the script can bind and fan out
  over. The child runs off the parent's critical path — the one shape
  in-exchange concurrency cannot express, the parent exchange ending before
  the child does — an *exchange* on this page being a human round, the fleet's
  own clock, never a unit of the context, which has only turns and their roles
  ([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]);
- **wakes the parent when the child replies** with a one-line notice through
  the parent's [[map/exarch/frontend|inbox]] — the parent edge `parent` names.
  The value itself stays on the child's own `Agent` (`Agent::status.reply`,
  written by the child's avatar alone) until the parent fetches it with
  `` exarch-agents `read <name> ``
  ([[decisions/260826_reply-parks|reply-parks]],
  [[decisions/260827_agent-and-avatar|agent-and-avatar]]).

The spawn's **`type`** field chooses the child's **model memory**, not its shell
isolation ([[decisions/260702_subagent-memory-modes|subagent-memory-modes]]):

- **`` `amnemon ``** is tabula rasa. The child starts with no conversation history;
  only the shell value-snapshot and the chosen prompt cross the edge. It has no
  ancestry: its transcript is its own and nothing else's.
- **`` `mnemon ``** remembers. The child imports the parent's model-visible context
  and appends the call's `prompt` as a fresh final user prompt. Left on the
  parent's own selection it reuses that provider's prompt cache; sent to
  another account or model it is still sound — reasoning crosses the transcript
  as plain text (`ContentPart::ReasoningContent`), not as signed blocks — but
  forfeits the cache. If the parent is mid-tool-call, the unanswered assistant
  tool-call frame is
  not inherited; the child forks the request context, not a dangling protocol.

Inheritance is **the parent's table, under the parent's own ids**. Ahead of
everything goes one `Protocol::Inherited { turns, notes }` — the parent's whole
turn table, each row `kind: Inherited`, `held` as the parent had it and
carrying the address of its transcript copy, beside the notes made at the cuts
that emptied those rows. The child then records one
`Protocol::ContextMessage { id, message }` per message under the parent's own
turn ids and roles, re-recording the resident turns as its
own: `` exarch-context `survey `` shows them as individual `import` rows, and
`` exarch-context `evict `` names any of them by turn id, exactly as it names the
child's own. Nothing
marker-shaped crosses by value — the child's markers, survey and index are the
same projections of the same fold
([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]],
[[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]).
That link is what makes the two logs **one transcript**: an id at or below the
reach
resolves against the ancestry, walking each ancestor file once into a memoised
index and following that ancestor's own link on to the grandparent, so a
`mnemon` child can `` exarch-transcript `read `` or `` `grep `` anything its lineage
ever recorded, evicted from the parent's context long before the fork included.
Ids are therefore lineage-monotone: the child mints its first prompt above the
parent's floor, and along any lineage an id names exactly one turn. The
chain breaks only where an ancestor's file has been taken away by hand, and
the refusal names the ancestor and the path rather than calling the turn
unrecorded ([[decisions/260906_context-rollover|context-rollover]]).

The spawn's **`provider`** and **`model`** fields choose what the child runs
on. ral has no optional record field and no null: absence is *data*, carried
by a variant ([[invariants/optionality-via-variants|optionality-via-variants]]),
so both fields are required and their values carry the optionality —
`` `inherit `` or `` `named <Str> ``. The record row therefore stays closed,
and a misspelled field stays a static error. The four rows:

| `provider` | `model` | the child runs on |
| --- | --- | --- |
| `` `inherit `` | `` `inherit `` | the parent's `Arc<Provider>`, shared verbatim, allocating nothing |
| `` `inherit `` | `` `named m `` | the parent's account and credential, model `m` |
| `` `named p `` | `` `inherit `` | account `p`: the parent's model if `p` is the parent's own account, else `p`'s service default model — refused, naming `model`, when that service has none |
| `` `named p `` | `` `named m `` | account `p`, model `m` |

Tuning (effort, temperature, `top_p`) and the output cap are the operator's
knobs rather than part of a model's identity, so they inherit in every row; the
`OpenRouter` route names a serving provider and survives only where the
resolved account is the parent's. **No catalog, no network, no inference**:
because `` `inherit `` *states* which account the child is on, a bare `model`
never has to be attributed to one, so a spawn can never block the fleet on a
model-list round trip, nor be refused because a cold catalog left a name
unattributable. That is the one deliberate divergence from the CLI, where
`--model` with no `--provider` must work out which account serves it — an
inference that exists only because a human typed no provider at all, whereas a
spawn always types one.

`provider::Bureau` is what makes any of this possible: `Provider::build` needs
an engine and a credential that no `Provider` retains, so the bureau names that
trio once and is the only place a live provider is minted
([[map/exarch/provider|provider]]). A scripted session holds `Bureau::Scripted`
and refuses a named selection in one sentence saying so.

### Bind and hand: context as a value

Selective delegation is ordinary data flow, not a new memory mode. The parent
surveys and reads closed turns, binds the returned records without
printing them, slices or reshapes them in ral, evicts the originals, and hands
the binding to an `` `amnemon `` child:

```ral
let ctx = exarch-transcript `read [turns: !{range 4 9}]
let handoff = take 12 $ctx[0][messages]
exarch-context `evict [turns: !{range 4 9}, note: 'handed to `researcher`']
exarch-agents `start [
  prompt: "read `handoff` for the material to work from; report your findings",
  name: 'researcher',
  type: `amnemon,
  grant: `read-only,
  search: false,
  provider: `inherit,
  model: `inherit,
]
```

`handoff` crosses in the child's forked scope, not in `prompt`'s text: `prompt`
stays a short instruction naming the binding, so the twelve messages travel
once, as data, rather than twice — once as a binding and once spliced into the
first model-visible message. The child receives `handoff` through the ordinary
serialisable value snapshot;
the same idiom can recur down the delegation tree. The token cost is paid at
the leaf for the slice that is actually handed over. Small, certainly needed
material may still be spliced into a prompt; a large binding should remain a
binding, since binding is silent while stdout and a final shell value become
model material.

## Wire-seat children: the same snapshot, a different wire

A `Seat::Wire` trunk ([[map/exarch/agent|agent]]) runs its shell in a guest
engine, host-side of a vsock connection
([[design/engine-protocol|engine-protocol]]) — the desk
that answers `` exarch-agents `start `` sits in the host process, and cannot reach a
`Shell` living in another machine the way it reaches into its own nursery.
**The whole of that asymmetry is one field of one enquiry.** Beside the model's
record, `` `start `` carries a `fork` tag saying how the child's forked session
is to be reached: `` `parked <id> ``, a nursery slot the handler adopts by id,
or `` `listening [port, token] ``, a port the guest has bound. Both arms run
every authority check the desk runs before anything is spawned, and both answer
the same roster, so the enquiring builtin cannot tell which one served it.

**The connection is opened by the side that knows what it is for.** A wire
spawn is one exchange:

- the builtin **binds an ephemeral guest port for the duration of this one
  spawn**, packs the scrubbed fork into an `EngineSeed` before the listener
  thread starts — a `Shell` never crosses to it, and the seed is all it ever
  holds of the parent's session — mints eight token bytes, and names port and
  token in the enquiry. The seed carries the spawn's `grant` as a
  `SpawnGrant`, and a `` `restrict `` record crosses it **undecoded**: the
  sigils in it must freeze against the child's own working directory, and the
  guest is the side standing in it
  ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]);
- the desk, *while answering*, dials that port, writes the token, and waits;
- the listener thread checks the token, spawns `current_exe --engine` with the
  dialled connection on its protocol fd and the seed on an inherited one — the
  same re-exec shape a wire seat's own construction uses, one level in — and
  only **then** writes the acknowledgement byte;
- the desk reads the ack, adopts the stream as a `Seat::Wire` child exactly as
  the trunk itself was seated, and hands it to the same `spawn_async` an
  identity trunk's fork reaches.

**A dialling side never has to ask which connection this is.** Correlation is
the price of accepting: a listener anyone may dial must publish a preamble for
callers to name themselves in, hold a table keyed by what that preamble
carries, run a pump to accept into it, expire the entries nobody claims, and
split the spawn into a phase that reserves a child and a phase that redeems it.
Open the connection from the other side and none of that has anything left to
do — the host dialled a port it was told about, in the enquiry it is still
answering, so it already holds the spawn the connection is for. What is left is
not a cheaper correlation mechanism but no correlation at all
([[design/engine-protocol|engine-protocol]]). The token
changes job with the direction: no longer a key naming a rendezvous, it is a
guard — the guest's proof that the connection it is about to hand a seeded
engine was opened by the host, and not by something inside the guest racing it
for the port ([[map/core/io-process|the spawn jail]] permits
`socket(AF_VSOCK)`; the guest kernel's refusal of a guest-local dial is the
standing defence, and the token stands behind it).

**The child exists before the roster names it.** The listener acks only after
`spawn()` has returned, and the desk enrols the child in the fleet only after
reading the ack. There is no window in which an enrolled agent has no engine
behind it — which is why the roster's `state` column is derived from the
agent tree at listing time, and why `` exarch-agents `list `` reads as a fact rather
than as an intent.

A spawn that fails leaves nothing to reconcile. A desk that refuses never
dials, so the builtin wakes its own listener thread rather than leave it in its
poll; a child that will not spawn is never acked, and the reason raised is
whichever stood nearer the failure — the guest thread's if it has one,
otherwise the desk's. Nothing was reserved, so nothing has to expire. Past the
ack the child is alive, and a refusal from there on simply drops the stream:
the child reads EOF on its protocol fd and the guest's own table reaps it.

Depth beyond one needs nothing extra: a wire child inherits the dialler its
parent was built with, so a helper spawning a helper of its own binds its own
port and is dialled exactly as it was, and `fuel` bounds the recursion as it
does in the in-process lattice.

**The one snapshot law: an identity fork and a wire hatch give the child the
same scope, handler stack and hooks, every name resolving to the same value or
the same absence.** A handle is what stands in the way — live authority over a
parent-side worker, with no wire form — and it is exactly the class of
authority every other line of the fork law already denies a child: the parent's
cancel domain, its terminal, its inbox, its provider handle. So the scrub lives
at `Shell::fork_scrubbed`, the one fork both arms take — the identity arm parks
it in the nursery, the wire arm packs it into an `EngineSeed` — and each handle
becomes an `` `opaque `` placeholder wherever it stood: in a binding, in any
scope a closure captured, in a native's applied arguments, in a handler frame's
arms. The fork empties the hooks, which the wire does not carry: they are the
installing host's lifecycle entry points, and a child engine is not that host.
A shell that reaches no handle forks as itself, its scope shared rather than
copied. A round-trip test pins the law: fork a shell in memory, seed the same
shell through `EngineSeed`, and compare bindings, captured scopes, applied
arguments and handler frames, with no hook in either child
([[map/core/transport|transport]]).

Past that one field the seat asymmetry ends and the fleet's uniformity resumes:
`` exarch-agents `message ``, `` exarch-agents `cancel ``, and the idle-lease reaper all
resolve a descendant by name through the fleet, then scope-check it with a
climb (`Agent::descendant`), whatever seat it sits on,
and a wire helper's `message` crosses as an enquiry on its own connection
exactly as an identity peer's does — sender and recipient never learn each
other's transport.

## Returning: the deliberate `reply`

A returning agent hands back the argument of an explicit **`` exarch-agents `reply ``**
call ([[map/exarch/builtins|builtins]]) — never a scrape of whatever prose ended
the run ([[decisions/260622_agent-reply-tool|agent-reply-tool]]). It is the
*sole* return path: a returning agent that finishes without it is re-nudged
within budget, then **fails honestly** rather than handing up a trailing
fragment that masquerades as the answer. The payload is the faithful
first-order ral value the model passed (`FOValue`): a headless trunk's goes to
the harness as JSON through the `user_json` projection
([[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]);
a child's is **deposited on its own `Agent`** (`Agent::deposit_reply`, the
one write `Agent::status.reply` ever takes) and answered, as a value, to
the parent's `` exarch-agents `read <name> ``
([[decisions/260826_reply-parks|reply-parks]],
[[decisions/260827_agent-and-avatar|agent-and-avatar]]). The parent learns of
the reply from a one-line inbox notice, never the payload.

A child's `reply` does not end it. Once the enclosing `ral` call's batch
drains, the child cancels and reaps its proper descendants — a parent may
abandon unfinished children, but never leave live agents registered beneath a
node that has answered — and **parks**, waiting for a message under its idle
lease. `` exarch-agents `message `` wakes it into a new exchange; a later `reply`
overwrites the deposit and notifies again. Only a non-reply finish — failure,
turn cap, cancellation — settles the entry at once, with its one-line tag.

## Focus is presentation; the idle lease is lifecycle

The human attachment is a presentational `AgentId` the frontend alone owns,
read by neither the fleet nor `park_mode`. Matrix navigation is a second such
value, and the matrix's only retained state: an agent identity, never a row
number, so a cursor whose agent has gone reads as the attached tab rather than
stranding off-list. `TAB` enters and leaves the surface, `↑`/`↓` (and
Shift-Tab) move the cursor, and `Enter` attaches the human to the cursor's row
*and* leaves navigation, so attach-and-type is one gesture. Navigation is
modal: while it owns the keyboard `Esc` leaves the surface rather than
cancelling the focused exchange — `Ctrl-C` alone still interrupts — and no
other key reaches the draft. The attached tab receives typed lines; moving
either cursor and looking at a tab keep nothing alive. What keeps a
non-conversing returning child alive past quiescence is a renewable **idle
lease** the fleet arms at birth for every parented agent (`Fleet::enrol`; one
hour, `AGENT_LEASE_IDLE`, a fleet-level bound rather than a per-agent field),
not the human's attention: the one thing that renews it is a delivered
*message* — a human's typed line (`Mailbox::steer`) or the parent's
`` exarch-agents `message `` (`Agent::message`) — both of which stamp the recipient's
own inbox exchange clock before the item is pushed, so a child that is being
talked to, and a child parked on a deposited reply, keep their lease fresh
(`Agent::idle`, read off that same clock), while a lease that is never renewed
fires at exactly its birth-seeded hour. A `/branch` child and the trunk are
roots — `Fleet::enrol` arms no lease for either — and so never idle-reap.
Neither matrix navigation nor a `/resources` probe touches the exchange
clock — enumeration and attention alone can never immortalise a child.

A leased child that is parked waiting for input and has sat idle for five
minutes demotes in place to a compact slate matrix row carrying its idle age —
a per-frame projection off the inbox's own exchange clock, never stored state,
and root is never a candidate. It keeps its position in the spawn tree: the
matrix draws `├─`/`└─` branches from the complete forest before clipping its
window, so an off-screen sibling cannot change a visible connector. The window
itself is a total function of the cursor and the fleet: the same pair always
draws the same strip, so no frame's height leaks into the next. It always
contains the cursor's own row, and it spends one line on each side it hides,
stating how many agents lie there. A surface a cursor moves through cannot be
invisible, so while navigation owns the keyboard the strip is drawn whatever
the frame's height, taking a row from the transcript if it must; watching, it
fits in whatever the transcript's floor leaves over. `/focus <name>` remains
the direct textual attachment. Every one of these gestures is presentation only
and never touches the lease. When the lease itself runs out, the reaper cancels
the whole subtree at the bound whether or not a human happens to be looking at
it — mere focus was never immunity.

## Descendant messages: marked notes, not shared memory

An agent may send a **marked message** by **name** to a proper descendant
through `` exarch-agents `message ``. The fleet resolves the name to the recipient's
`Agent`; `Agent::message` stamps its exchange clock and posts an
`AgentMessage`; the recipient sees it at the
next tool boundary as a marked note naming the sender, not as human input. This is
coordination, not a return edge: it does not share shell state, does not grant
authority, and does not wait for an answer. The durable result path remains
`reply`; the durable cancellation path remains `` exarch-agents `cancel ``, addressed
by name the same way.

## Cancellation: a key interrupts one exchange; a terminator cascades the subtree

`Esc` and `Ctrl-C` are a **per-tab exchange interrupt** — they unwind only the focused
tab's current exchange, never a subtree and never an agent
([[decisions/260705_cancel-per-tab|cancel-per-tab]]). The subtree cascade survives,
but only behind the **lifecycle terminators**: the `` exarch-agents `cancel `` tag, the
per-agent idle-lease reaper, and `/clear`. They share one cascade over the
agent tree itself (`Agent::parent`/`Agent::children`), so terminating a
mid-tree agent (`Agent::cancel_tree`) reaps everything below it
(`Agent::cancel_descendants`, a walk over `children`); `/clear`
(`Agent::clear_subtree`) additionally drains this agent's own inbox, whose
clear-epoch bump drops a late result or deferred surface batch addressed
into the rebuilt context. A `reply` still
cancels only the replier's proper descendants — a
parent may abandon unfinished children, but never leave live agents registered
beneath a node that has answered. This refines
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]: the per-focus cancel
token interrupts one exchange in place, while the subtree cascade is the
terminators' alone.

In the TUI, that break is a distinct cancelled rail shape: it wears the `╳`
marker used for errors, while the matrix's failure cell remains reserved for
actual failures.

## Self-scheduling is inherited

An agent may arm its own wakeups (a cron expression or `after <dur>`) into its own
inbox when the trunk was launched `--allow-schedule`: the grant is
**inherited by a fork**, so it flows down the spawn tree. Scheduling is
gated by that authority — refused at the desk without it, and the schedule
family is dropped from an ungranted agent's builtin index
([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]). A live self-schedule
parks its agent `ParkMode::UntilCancelled`: waiting for the wakeup, but stopped
by a terminate-cause cancel.

## Permissions: a spawn pushes one layer

**Every spawn states the child's authority explicitly**, through a *mandatory*
`grant` field with six spellings:

```text
  grant: `inherit | `confined | `read-only | `edit-only | `reasonable | `restrict R
```

Four name a capability bake-in (the bake-ins are ordered loosest-to-tightest in
[[map/exarch/policy|policy]]); `` `restrict R `` states a capability record
outright, written in the parent's shell in exactly the vocabulary of
`grant [...] { body }`; `` `inherit `` names ⊤, a layer that says nothing. An unknown tag,
or an `R` with an unknown key, is refused at the runtime door in the declared
table's own words ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]).

**One field, because a spawn has one question.** At the CLI, `--base` and
`--restrict` are different acts: a base *establishes* a ceiling with nothing
beneath it, a restrict *narrows* what is already established. At a spawn there
is nothing to establish — the parent's `GrantStack` is already underneath — so
a named base is pushed *as if it were a restrict*, and both acts collapse into
"what single layer does this child get?". The CLI keeps its two flags, because
there the two acts genuinely differ; the mirror worth having between the two
surfaces is in the vocabulary of values, not of flag names.

The child is born with its parent's whole stack, plus at most one layer:

```text
  child_stack = parent_stack ++ [layer(grant)]
```

`policy::base_layer` (the installer's `GrantNarrower`) resolves a named base,
`decode_capability_map` an `R`, both frozen against the **child's** working
directory so the layer lands already resolved
([[design/capability-freeze|capabilities]]). The fork carries the parent's
stack, and `SpawnGrant::narrow_onto` pushes the layer onto it: at adoption for
an identity child (`IdentityTransport::adopt_parked`), at `EngineSeed::apply`
for a hatched one ([[map/core/transport|transport]],
[[map/exarch/policy|policy]]). There is no meet and no `Capabilities`
folded against another: the stack's own per-check fold ANDs every layer's
verdict, so a pushed layer can **narrow** the child below the parent and can
**never escalate** it past the parent's reach
([[design/grant|the grant lattice]], [[decisions/260906_object-not-name|object-not-name]]):

- naming a base *looser* than the parent simply changes nothing — a network-off
  `confined` parent stays offline even under `reasonable`, since the fold ANDs
  both layers' verdicts;
- an `R` claiming authority the parent does not hold is ANDed away for the same
  reason, so it needs no comparison against the parent to be safe;
- `` `inherit `` resolves to ⊤, which the fold leaves no trace of, so the child
  runs at the parent's own ceiling.

`` `inherit `` is the one spelling the CLI has no name for, and the spawn
surface has no `` `dangerous ``: at a spawn the lattice top is a layer that says
nothing, which is *inherit the parent verbatim* under a name that claims
otherwise. `` `inherit `` says it plainly, in the spelling `provider` and
`model` already use for *no opinion*, while `` `dangerous `` stays a `--base`
name, where ⊤ genuinely is no ceiling.

The four bake-in names are a *subset* of the `--base` vocabulary, and the
door's own label list is where the two part company. A grantable base must admit
the bundled coreutils (`ral_core::uutils`), which spawn by bare name and so
match no directory prefix: `read-only` and `edit-only` name each tool literally
for exactly this reason. A base whose `exec` block is prefixes alone leaves the
child unable to run `ls`, and — the ceiling being non-escalating — with no way
to ask for it back. A human at the CLI can see that and reach for
`--extend-base`; a child can only spend turns discovering it.

A spawn's `R` mints no self-denial. `--restrict` denies the restriction
*files'* own paths, so the agent cannot rewrite the bytes that shape its
permissions; an `R` is a value computed in the parent's shell at the instant of
the spawn, with no file for the child to reach.

The authority decision is the spawn site's, not the child's: the desk carries
the spawn's `SpawnGrant` into the child's seating (`ExarchDesk::fork_seat`), and
the child receives only the layer it resolves to.

## See also

[[design/exarch-architecture|exarch-architecture]] (the agent as a provider loop
over one `ral` tool),
[[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]
(the record-spec `` exarch-agents `start `` tag, names as fleet-unique identity,
schedule labels, commitments retired),
[[design/grant|grant]] (the capability lattice the fold runs in),
[[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]] (why `grant` is
one field with six spellings, and why the CLI's two flags are not),
[[map/exarch/tools|tools]], [[map/exarch/agent|agent]],
[[map/exarch/policy|policy]],
[[decisions/260617_async-agent-tool|async-agent-tool]],
[[decisions/260622_agent-reply-tool|agent-reply-tool]],
[[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]],
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]],
[[decisions/260705_cancel-per-tab|cancel-per-tab]] (Esc/Ctrl-C are a per-tab exchange
interrupt, not a subtree cascade),
[[decisions/260705_branch-minimal|branch-minimal]] (the conversing child whose
`returns` bit is fixed false at construction),
[[design/engine-protocol|engine-protocol]] (why the guest
listens and the host dials, what that direction deletes, and the enquiry
class per registry, every tag answering the registry's state),
[[decisions/260827_agent-and-avatar|agent-and-avatar]] (`Agent` as the `Arc`,
`Avatar` as its embodiment, the fleet as `{ names, roots, lease }`),
[[map/synod|synod]] (the dialler's landed home, and the helper surface built
over wire-seat children),
[[decisions/260806_exchange-ends-at-fleet-quiescence|synod's exchange ends at
fleet quiescence]] (the product law a wire-seat fleet's caller must satisfy).
