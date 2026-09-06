# Audit

**ral records execution as a flat, lexically-scoped trail.** The `audit`
operator runs its body and returns a **report** — how the body settled, beside
the trail of what happened: commands, redirect reads and writes, capability
denials, worker births — so a run can be inspected after the fact. It is one of
the five [[design/control-operators|control operators]] for the same reason as
the others: the trail it threads lives in `Shell` state, not in any value the
body could construct. Every fact is one vocabulary, `Observation`
(`core/src/types/observation.rs`) — the same one the surface rail and
`--audit`'s JSON project, and the same one a host author speaks (exarch's
desk records its committed acts as `Observed::Act`, host-side, into a
per-call fragment of its own rather than the engine's trail) — so a command,
a write, a read, a search, a capability check, a worker's birth, and a host
act are one shape apiece rather than seven private ones.

**The report has two fields, and neither is a second copy of something else.**
`outcome` is `` `ok `` of the body's value or `` `err `` of *exactly* the error
record `try` hands its handler — so `try`, `poll`, and `audit` report failure
in one vocabulary, and a reader that can read one can read all three. `trail`
is the list of observations. Nothing else: no status field duplicating the
process exit status the host already owns and `audit` never changes, and no
hint appended to the message, because a hint is advice for a person and a
report is data. An ordinary failure settles into `` `err ``; an `exit` escape
is not a failure and walks past the report entirely
([[design/failure|failure]]).

**The kind is the tag.** An observation is an envelope — `script`, `line`,
`col`, `start`, `end`, `principal` — around one `what`, a variant over
`` `command | `write | `read | `grep | `check | `worker | `act ``. There is no
`kind` string beside it, so there is no second place for the kind to be
recorded and nowhere for the two to disagree; a reader dispatches with `case`
and the typechecker decides exhaustiveness. `` `grep `` and `` `act `` are
raised by host doors, never by a door in core.

The two questions almost every reader asks — *did it work* and *what ran* —
are prelude functions rather than idioms to be rediscovered: `succeeded
$report` and `commands $report`, the latter handing back the `` `command ``
payloads in order. A `case` on `what` is for the reader who cares about the
other six tags.

Retention is one rule for both byte streams: `stdout` and `stderr` are each
capped at the same 16 MiB as a worker's buffers, with a truncation marker at
the cap. Neither is privileged, and neither cap is an I/O limit — every byte
still reaches its ordinary destination, since auditing is observational and a
tee is not a gate.

**Collection is lexical, not temporal, and the trail itself is flat.**

- Two sites delimit a trail, and only two: an `audit { }` and the run door
  (`ral --audit`, or a dispatching host's tool call). Each owns the trail its
  body produces — it opens or inherits collection, and everything the body
  observes lands in that same flat list. Neither builds an observation of its
  own; they are collection boundaries, not entries in the trail.
- The other operators — `within` / `grant` / `guard` / `try` — delimit
  nothing. They are transparent to collection, so their bodies' observations
  land in the enclosing trail, and a `try` in particular costs no trail at
  all: nothing it does needs one.
- Source nesting decides which collection an observation lands in, regardless
  of which process or thread produced it.
- A process boundary — the OS-sandbox child a `grant` re-execs into
  ([[design/grant|grant]]), a pipeline-stage helper — only *transports* its
  fragment back to the owning evaluator, which merges it into the surrounding
  trail; the boundary never decides structure.
- A delimiter's lifecycle is scope-shaped, not one-shot: opening either
  installs a trail or finds one already open, and whichever happened decides
  what closing does. The delimiter that installed the trail drains and closes
  it on every exit, panics included, so the next dispatch starts from nothing;
  a delimiter that found one already open reads back only its own suffix and
  leaves the trail intact for its opener to close later. Nobody but the
  opener closes — that discipline is what keeps a nested `audit { }` from
  leaving the trail standing open for the rest of the session.

So a sandboxed `grant { … }` merges its body's commands into the nearest open
trail rather than losing them at the process boundary, and a transported
fragment stays self-describing without needing a parent to interpret it.

**Within a trail, position is settlement, and nesting is containment.** An
observation is appended when the fact it records becomes *true*, not when the
construct that produced it began: a command's own observation therefore lands
after those of everything it ran, and a redirect's `` `write `` lands when the
write settles. The order is a post-order traversal of what happened, and it is
the one order a flat list can carry without lying — an in-order list would have
to claim a command was finished before the work inside it was.

Nesting is then recovered from the timestamps: one observation's `start`–`end`
interval contains another's exactly when its extent contained the other's.
That is why the flatness costs nothing. Containment needs no parent pointers,
so it survives a fragment's trip back across a process boundary, and it holds
of a fragment merged out of order just as well as of one recorded in place —
where a parent-pointer tree would have had to be stitched back together by
whoever received it.

Each observation is self-describing about who and how it happened:

- every observation carries the `principal` in force where it was recorded, so
  the trail records *who* as well as *what*, and a transported fragment still
  names its actor — `None` in Rust, the empty string in the projection, where
  no `$USER` is bound and there is nobody to name;
- an observation carries only its own tag's fields — a `` `command ``'s
  `argv`, a `` `check ``'s `resource` / `decision` — never a handler frame or
  capability map, and never another tag's fields. A `` `command `` carries no
  returned value either: `argv`, the status, and the bytes are what a later
  reader can act on, and a process-local or executable value has no honest
  projection to offer one;
- an optional field is a variant, `` `just `` or `` `none ``, never a missing
  key: "there was no before-image" is a fact the trail states, not one a reader
  infers from silence. A before-image is a *read*: under a grant that admits
  no read of the target, the write still lands and `old_bytes` is `` `none `` —
  decided at the door (`Shell::admits_fs_exact`), before the bytes could enter
  any observation, never hidden afterwards by a renderer;
- tail-recursive iteration adds no wrapper: a loop contributes one flat run of
  observations, not a chain as deep as the iteration count, matching how
  [[design/scoping|dynamic scope]] persists across tail calls.

What builds up the trail is itself scoped:

- plain execution records nothing unless a host asks: a dispatching host
  (exarch's tool call) may open its own delimiter over the whole run —
  `Run.trail: Some` in the transport protocol — and get the extent's trail
  back on the `Report`, each observation in the shared `Observation` map
  shape, with an `` `opaque `` placeholder wherever a value has no wire form;
  the REPL never asks, and asking costs nothing beyond what the surface rail
  already builds per command;
- `try` reads nothing. The failing command's name is stamped on the error by
  the dispatch that failed, so the innermost failing dispatch wins the way the
  innermost source span does, and `try` costs no more than its own frame. It
  never had a reason to consult the trail: a name recovered by scanning
  observations is a guess about which of them was the cause, whereas the
  dispatch that raised the error *knows*. This also decouples the two — the
  error record is complete whether or not anyone is collecting;
- a `grant` has no say. Whenever a trail is open, a capability *denial* is
  recorded as a `` `check `` observation and an allowed check never is: the
  denial is a fact about authority nothing else in the trail attests, while
  everything an allowed check would attest is already attested by the command,
  read, or write it let through. There is no flag to set and none to clear, so
  no inner grant can hide a denial from the trail its caller opened. A denied
  *head admission* also reaches the surface rail, whether or not a trail is
  open; an `fs` or full-argv denial reaches the trail alone — the command it
  refused still surfaces in its own right, as a failed `` `command ``
  observation carrying the denial message;
- `audit` collects the full trail its body produces.

**A write that changed nothing in the world is not a fact.** A redirect onto
the discard device — `/dev/null`, or `NUL` on Windows — records nothing: no
card, no rail barrier, and no line in an agent's trail claiming it wrote a
file. One predicate says so, `ResolvedPath::is_discard`
(`core/src/path/resolved.rs`), asked at both doors that have an opinion: the
capability gate, which excuses such a target from an *access*
([[internals/capability-enforcement|capability-enforcement]]), and
`observe_stamped` itself, the one fan-out door, which excuses it from a
*mutation*. Every redirect seam already passes through that door, so the rule
is stated once and no seam can forget it.

See also [[design/syscalls-are-effects|syscalls-are-effects]] — an audit trail
is a trace of the operations performed and the scopes that framed them.

Recording lives in `core/src/evaluator/audit.rs` ([[map/core/evaluator|evaluator]]);
the dispatch delimiter is the run door's own scope, held at `Shell::enter` in
`core/src/run.rs`, outside the `catch_unwind` a panicking run rolls back
through — a panic still drains and closes the scope, but reports `Static`
rather than carrying a trail; `docs/SPEC.md` §13.3 gives the formal
account.
