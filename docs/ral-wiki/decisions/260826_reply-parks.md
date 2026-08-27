---
status: accepted
---

# Reply parks

> Superseded, in part, by [[decisions/260827_agent-and-avatar|agent-and-avatar]]:
> there is no registry entry. A reply lives on `Agent::status.reply`,
> written by the avatar alone and read by anyone still holding the `Arc`.
> The decision below — deposit and park, never terminate — stands.

**A returned value is a fact the registry holds, not a message the parent
reads.** A returning agent's `reply` deposits its value on the agent's own
registry entry and parks the agent; the parent is woken with one line and
fetches the value, when and if it wants it, with `` agents `read <name> ``.
The replier stays — hidden, quiesced, under the idle lease — and a message wakes
it into a new exchange. This supersedes the *terminating* half of
[[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]];
its other rulings — `reply` is mandatory, advertised to every returning agent,
and its payload is the faithful `FOValue` — stand.

## The complaint

Under 260623 a child's reply was rendered to prose and pushed into the parent's
inbox as `[agent 'x' finished]\n<text>`, and the child's entry was `settle`d
away. Two things were wrong with that, and they are the same thing seen twice.

The payload arrived *unasked*. A parent that spawned three explorers to keep
their working out of its context got three reports dumped into it, in full, at
the moment each happened to finish — exactly the flooding the spawn was meant
to avoid. And the reply was *prose*: a structured verdict the child built as a
record crossed the edge as text, to be re-read rather than bound.

The child was *gone*. The one agent that knew the answer's provenance could
not be asked a follow-up; the parent's only recourse was to spawn again and
re-explain. Yet the machinery to keep it was already there — the one-hour idle
lease, the five-minute tab demotion, the `Engaged` park — armed only for a
child a *human* had typed at.

## The decision

- **`reply` deposits and parks.** The desk stages the value as before; when
  the batch drains the child writes the `FOValue` to its registry entry,
  cancels its proper descendants (a reply still means "this subtree's work is
  done"), and parks waiting for a message. Cancel and the idle lease end that
  park as they end any `Engaged` one; a non-reply finish — `Failed`,
  `Stopped`, `Cancelled` — stays terminal and keeps its one-line tag.
- **The parent is woken, not fed.** The child posts a notice that renders as
  one line, `[agent 'x' replied — agents `reply 'x' fetches it]`. A parent
  parked on its children needs *some* wake; the payload is not it.
- **Both verbs live in the `agents` family; the standalone `reply` builtin
  is gone.** The child hands up with `` agents `reply <value> `` (refused on a
  non-returning agent, keyed on `returns` as before; answers the roster like
  every other tag). The parent fetches with **`` agents `read <name> ``**,
  which answers the record `[name: Str, reply: <the deposited value>]` — a
  string reply is a `Str`, a record a record — so `let r = agents `read 'x'`
  binds it and the rest is ral. It is idempotent until the child replies again
  or is reaped; a child that has not replied refuses with a sentence saying
  so. It is the one tag that does not answer the roster, because its answer is
  not the fleet's state but one agent's value.
- **The family's type pays for `read`.** `agents` was
  `<…> → F roster` because every tag answered the roster; `read` answers a
  value of unknown shape, so the scheme becomes
  `∀α β ρ. <list | start … | message … | cancel Str | reply β | read Str | ρ> → F α`
  — the `pin-read`/`from-json` precedent. `list` and `start` still answer the
  roster at runtime; they are no longer *checked* to. Accepted knowingly over
  a separate pin-read-shaped verb, to keep one word for the fleet.
- **The roster says what each agent is doing.** Rows carry `state` — `` `busy
  ``, `` `waiting-on-agents ``, `` `replied ``, `` `waiting `` — and `idle-s`,
  the seconds since the agent parked. Both are derived at listing time from
  registry facts, so the roster still reads as a fact, not an intent.
- **A parent holds for *busy* children, not live ones.** `HeldByChildren`
  now reads "some child has not parked". Otherwise a parent whose children
  had all replied would wait out their hour.
- **Any message renews the lease.** The 260623-era rule that only a *human*
  message renews is refined: a *message* — the parent's `` agents `message ``
  or a human's typed line — is an exchange and renews; focus, listing, and
  `/resources` probes still do not. Attention alone still never immortalises
  a child.
- **Nobody quiescent is badgered.** The nudge layer has one gate for every
  nudge kind — the reply reminder, the pin reminders, context pressure: an
  agent is nudged only if it has not replied, is waiting on no detached shell
  work, and has no busy children. A standing reply stands; a wait is a wait.

## What follows

Synod's Law B
([[decisions/260806_exchange-ends-at-fleet-quiescence|exchange-ends-at-fleet-quiescence]])
reads "no live children" as "no busy children": an exchange ends once every
child has replied or died, while the repliers linger under their lease.

The reply prompt ([[map/exarch|`data/reply.md`]]) no longer tells the agent
that `reply` ends its run: it may be woken by a message and asked for more, and
answers again with `reply`.
