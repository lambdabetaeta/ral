---
status: accepted
---

# The wire carries the value

**A registry is one enquiry class, named as the model names it. Its payload is
the model's own value. Its answer is the registry's state.** Nine private class
names become two — `agents` and `schedules` — with the old acts as tags beneath
them, and the model's records cross by field name instead of by position. The
desk's class count goes 16 to 9. No model-facing type changes at all.

## The founding complaint

[[decisions/260719_agent-names-and-schedule-labels|agent-names-and-schedule-labels]]
and the commit that followed it gave each registry **one verb** at the surface.
Underneath, the engine still disassembled that verb into a private vocabulary of
nine class names and re-encoded the model's records as positional lists. The
surface got its rule; the wire never did.

That left a **second vocabulary nobody needed**, and it leaked. The class names
were a dialect — `agent-start`/`agent-list`/`agent-cancel` carried a family
prefix, `message`/`schedule`/`schedule-list`/`unschedule` did not, and
`unschedule` broke the `<family>-<act>` shape twice over where `schedule-remove`
would have sat beside `schedule-list`. It read as history, not design. Worse, the
guard that refuses a schedule without the self-wakeup grant interpolated the
*class name* it was handed, so a model that typed ``schedules `list`` was told
off about `` `schedule-list` `` — a name it was never taught.

The records were flattened. The door had named fields and threw them away:
`` `add [trigger: …, label: …, prompt: …] `` crossed as a three-element list.
That was the whole reason the desk carried `payload_list<N>` and eight positional
accessors, and the reason its diagnostics spoke in positions where the door's
spoke in names. `FOValue::try_from(&Value)` already existed and its recursion
*is* the seam's first-orderness check, so the record could have crossed by name
for nothing.

And every transition cost two round trips: the builtin issued the act, then
re-read the roster, because the listing was a separate class. The rule that made
the surface one verb is what made the wire two calls.

## The decision

- **Two classes, tags beneath.** The nesting is not decoration: it is what makes
  sending the model's value possible at all. `` agents `list `` and
  `` schedules `list `` are both `` `list ``, and the family tag is what tells
  them apart. The prefixes existed to answer a collision that a second level
  answers properly.
- **The answer is the registry's state.** Every tag answers the roster or the
  table. The four outcome tags (`` `cancelled ``, `` `no-such-agent ``,
  `` `removed ``, `` `no-such-label ``), the `[label, next-s]` schedule receipt
  and the `` `started `` receipt all go — not because anything was taken from the
  model, but because the surface had **already** stopped reading them, so they
  had no reader at all. This drops a round trip from every transition, and the
  two layers finally say the same thing.
- **The refusal speaks the model's language.** The grant guard takes the tag, so
  the sentence names what the model typed.

## Three things a later reader will otherwise undo

**The desk's decode is a trust boundary, not a duplicate.** In wire mode the
engine and its shell run inside the VM and the desk runs on the host. Both
validations stay. The two `CronSchedule::parse` calls are *not* redundant: the
door's exists so a bad cron reaches the model with the parser's own message
before anything crosses, the desk's because a guest can send whatever it likes.
The tab-bar name contract is the same shape of claim answered one level deeper:
rather than have the desk recheck what the door already checked,
`AgentRegistry::register` refuses whatever `check_name` refuses, so a wire peer's
name cannot become registry identity unexamined *by any door*, present or future.
The door keeps its own early refusal for the model's sake; what makes it safe is
that it is not the only one. What changed is only that the host-side decode reads
the shape the model wrote, by field name, instead of a dialect.
Take "the wire carries the value" without
this and you will delete the check that keeps whatever a guest cares to send from
reaching host state unexamined.

**`` `start ``'s payload is not purely the model's value.** It is the model's
record *plus* the one field the engine minted for itself when it forked, because
the reentrancy law bars a desk handler from holding `&mut Shell`. It rides
**outside** the record — `` `start [spec: …, fork: …] `` — so the model's record
crosses verbatim and the engine's contribution is visibly the engine's. `fork` is
a sum whose tags the model wrote neither of, which is more visibly the engine's
than a bare id would be. See [[decisions/260825_the-host-dials-in|the-host-dials-in]]
for its two arms.

**The extension law gains a second axis.** Under
[[decisions/260706_enquiry-channel|enquiry-channel]] an unrecognised *class* drew
a loud error and never a silent default. That now holds one level down as well:
an unknown **tag within a family** draws the same error. The nesting must not
open a silent hole beneath the rule it was introduced under.

## What is untouched, and why it matters

**Every model-facing type.** The schemes, the tags, the closed payload records,
the open `type`/`grant`/`trigger` rows, both row types. `exarch/data/system.md`,
`ral.md` and `agents.md` needed no edit — checked, not assumed. This decision is
invisible from above, which is the point: the surface got its rule earlier, and
this is the wire catching up.

**`DeskAct` and everything downstream of it.** Its verbs are authored host-side
at the arm where the outcome is known, never by the engine, so they were never a
wire vocabulary. The rail's verb column, the unwind audit sentence, `RailKind`,
and `record.jsonl` all read exactly as before.

**The seven singleton classes.** `reply`, `pin-read`, `pin-list`, `context`,
`context-read`, `context-drop`, `context-fold` are already 1:1 with their builtin
names — no family to nest under, no dialect to retire. `payload_list` and the
scalar accessors survive for them. Their residue of the same habit is
`payload_list<1>`, a one-element list wrapping a single argument; unwrapping it is
a real cleanup and a separate one.

## Consequences

Amends [[decisions/260719_agent-names-and-schedule-labels|agent-names-and-schedule-labels]]
(the class inventory it left standing) and
[[decisions/260706_enquiry-channel|enquiry-channel]] (the extension law's second
axis). Leaves the closed *channel* set untouched.

The spawn half of the plan this came from was struck before implementation:
the two-phase wire spawn it designed around lost to a reversed dial, and the
`state` column that made asynchronous adoption expressible has no referent when
the child exists before the roster names it. See
[[decisions/260825_the-host-dials-in|the-host-dials-in]].
