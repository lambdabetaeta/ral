---
status: proposed
---

# Durable inside the process is not durable across it: work you stop owning is `detach`, born by double-fork

**`service` is durable *within a session* and cannot be made durable *beyond*
one, because its child's process group is recorded by the parent at birth and
signalled at teardown. A task whose grader runs after exarch exits is therefore
structurally unwinnable with `service` — and `service` is the primitive exarch
advertises for exactly that job.** The fix is not a knob, a lease class, or a
document: it is **`detach`**, the verb
[[decisions/260617_long-running-work|long-running-work]] deferred as **Regime
2**, built now that a concrete need has appeared, and born by **double-fork**
so that the surviving process's pgid is never observed by `RunningChild` at
all. The name is the content of the decision: the axis is not *how long the
work lives* but *who owns the process*, and `detach` is the only one of the
four concurrency verbs where you stop owning it. Three findings force this
shape, and the second is the one that should worry us most: the teardown that
kills a service today is not a designed kill but a **footrace** between the
worker thread's poll loop and the exiting main thread, whose outcome no code
guarantees in either direction.

Revives the Regime 2 half of
[[decisions/260617_long-running-work|long-running-work]] (superseded for its
Regime 1 half, which shipped as `service`); inherits the "birth, not promote"
rule from that page and the rejection of a lifetime knob on `spawn` from
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]].

## The finding that forced this

terminal-bench run `exarch-bench/terminal-bench/jobs/2026-07-25__02-04-52/`,
`exarch__openai/gpt-5.6-sol` against `mini-swe-agent__openai/gpt-5.6-sol`,
five trials each on terminal-bench-2-1. exarch scored a mean of 0.6. **Both
of its two zeros are the two tasks that require a live listener at grade
time**, and it passed all three tasks that do not.

On `kv-store-grpc__a4KpxWN` the model wrote, after explicitly looking the
builtin up with `explain service`:

```ral
let kv-server = service #'gRPC KV store server on port 5328'# { within [dir: #'/app'#] { python server.py } }
```

It then verified its own work twice, with live RPC round-trips, both exit 0
(`RPC verification passed`, and a second health check asserting
`SetVal("final-check", 5328).val == 5328`). The verifier, run afterwards in the
same container, got:

```
E  AssertionError: Port 5328 is not listening - no real gRPC server is running
E  assert 111 == 0
```

`111` is `ECONNREFUSED` from `connect_ex` — a kernel-level statement that
nothing was bound to the port. exarch exited at `01:14:04.29`; the verifier
started at `01:14:05.73`. The server was serving RPCs seconds before and
refusing connections ~2 s after exarch left. The only event in that window is
exarch's exit.

`pypi-server__kDbVfCT` is the same story with a weaker signature (pytest's
`saferepr` truncates the pip error, so ECONNREFUSED cannot be distinguished
from a mid-exchange abort from the verifier text alone). Its agent transcript
shows the same pattern: a `service`-born `python3 -m http.server 8080`, then
the agent running *the exact command the verifier would later run* —
`pip install --index-url http://localhost:8080/simple 'vectorops==0.1.0'` —
successfully, exit 0, `Successfully installed vectorops-0.1.0`.

### The controlled experiment

The same model under a bash harness passed `pypi-server` 1.0 against exarch's
0.0, having reasoned its way to the process boundary explicitly:

> *"I need to set up a server on port 8080 that persists. … It looks like I
> need to use `python http.server` in the background with `nohup` since
> commands may persist as subshell processes."*

But `kv-store-grpc` is the sharper result, because there **both** agents scored
0.0 — on *complementary halves*:

| | proto schema | listener at grade time |
|---|---|---|
| exarch | **correct** (`int32 value = 2`) | **dead** |
| mini-swe-agent | wrong (`int64 val = 2`) | **alive** |

The same model, on the same task, produced the *correct artifact* under exarch
and an *incorrect* one under mini. exarch did not lose because the model
reasoned worse; it lost the process it had correctly built. That is a harness
capability gap, stated about as cleanly as a benchmark can state one.

Neither exarch trial ever emitted `nohup`, `setsid`, `disown`, or a trailing
`&` — `grep -c` over both transcripts returns 0. The model was not fumbling for
an escape hatch. It believed it had one.

## What actually happens: the exact chain

The benchmark runs `exarch --headless --output-format json`
(`exarch-bench/terminal-bench/agents/exarch_agent.py:253`), so
`exarch/src/headless.rs:381` runs and `tui::run` never does — but the frontend
turns out to be irrelevant, since both return into the same tail of
`exarch::run` (`exarch/src/lib.rs:310-331`) and drop the same locals. All five
trials recorded exit 0: a clean return, not a signal.

```
exarch::run returns; locals drop                    exarch/src/lib.rs:310-331
  impl Drop for Agent                               exarch/src/agent/build.rs:575-597
  field `seat` → Seat → Shell → LocalState          exarch/src/agent.rs:108
  LocalState::drop → workers.cancel_all()           core/src/types/shell/mod.rs:542
  per entry: handle.cancel.cancel(Explicit)         core/src/types/shell/workers.rs:492-497
  AtomicU8 fetch_max(2)                             core/src/process/cancel.rs:139   ← flag only
  ── crosses to the detached worker OS thread ──
  poll loop reads the cause                         core/src/runtime/command/child.rs:430   [≤100 ms later]
  terminate_group(child, Explicit)                  core/src/runtime/command/child.rs:447
  pgid.signal_group(SIGTERM)                        core/src/runtime/command/child.rs:324
  libc::kill(-pgid, SIGTERM)                        core/src/process/signal.rs:395   ← the syscall
  500 ms grace_poll                                 core/src/runtime/command/child.rs:329-330
  kill_process_group(pgid, SIGKILL)                 core/src/runtime/command/child.rs:334 → :218
```

SIGTERM alone suffices for both servers: `python -m http.server` and
`server.wait_for_termination()` die on the default disposition.

### `setsid` is not an escape, and was never meant to be

A worker's external child *is* put in a new session —
`PgidPolicy::NewSession` is selected at
`core/src/runtime/command/foreground.rs:105-113` (the worker's `Io` defaults to
`LaunchRole::TopLevel`, `core/src/io.rs:52-53`, and `interactive == false`),
applied as `libc::setsid()` at `core/src/process/signal/unix.rs:275`. It is
tempting to read that as detachment. It is not, and the code says so at
`core/src/runtime/command/foreground.rs:91-92`:

> *The session's pgid still equals the child's pid, so the subtree
> `kill(-pgid, …)` is unchanged.*

The parent **records** that pgid at `core/src/process/signal/unix.rs:365-371`
and stores it on `RunningChild.pgid` (`core/src/runtime/command/child.rs:180`).
Both kill paths target *that recorded pgid*, never exarch's own group. Leaving
the session severs the controlling terminal — which is the point, a security
property recorded at `docs/SPEC.md:1914-1920`, so that detached work cannot
`tcgetpgrp` or signal the tty owner — and buys exactly nothing in lifetime.

**This is the single most important fact for anyone building `detach`**: the
thing to escape is not the session. It is the parent's *observation* of the
pgid.

### What does *not* run

`impl Drop for RunningChild` (`core/src/runtime/command/child.rs:644-676`)
would SIGKILL the group, and does not execute here: the `RunningChild` lives on
the detached worker thread's stack, and `exit_group(2)` destroys that thread
without unwinding. No worker thread is ever joined —
`core/src/types/shell/inherit.rs:306` is a plain `std::thread::spawn` and
`core/src/builtins/concurrency.rs:287` binds the `JoinHandle` as `_join` and
drops it. Nothing on the shutdown path sleeps, flushes, or waits: `Egress`,
`AgentLog`, `Transcript`, and `Scratch` (`exarch/src/bootstrap.rs:147-166`) all
have no `Drop`.

Note also that this run used `--base dangerous`
(`exarch/data/dangerous.exarch.ral`), so `core/src/capability/sandbox.rs:60-62`
returned `None` and no bwrap envelope existed —
`core/src/sandbox/linux.rs:64`'s `--die-with-parent` never applied. Under a
sandboxed base the kill would be deterministic and immediate, by a completely
different mechanism. **The lifetime of detached work currently depends on which
`--base` you booted with.**

## The kill is a race, not a design

`cancel_all` sets a flag. The worker's poll loop reads it at a backoff capped at
100 ms (`core/src/runtime/command/child.rs:395-396`, `:449-450`). Between the
flag and the read, the only thing keeping exarch alive is dropping the
remaining `Agent` fields, an `Arc<Provider>`, and the tokio runtime
(`exarch/src/provider/transport.rs:115`). **exarch does not block for the
grace period. It races it.**

On paper exarch should often win that race and orphan the child. Empirically it
loses it consistently — the two servers died within ~2 s. The most likely
explanation is that post-`cancel_all` teardown (tokio shutdown plus file-handle
closes onto a Docker Desktop bind mount) exceeds 100 ms in the container, but
this is inference, not verification; see Open questions.

Two properties follow, and both are bad:

- **`docs/SPEC.md:1907`'s third clause is true by accident.** "Dies only by
  `cancel`, by the host discarding its session context, or with the process" —
  the first two clauses are designed and hold (`cancel` works; `/clear` works
  at `exarch/src/agent/seat.rs:187-201`, precisely *because* the process
  survives long enough for the flag to be read). The third is a thread winning
  a footrace it was never told it was running.
- **The observable behaviour is not stable.** A faster teardown, a quieter
  container, a different filesystem, a machine with more cores — any of these
  can flip it. A harness that is *reliably* wrong is a bug; one that is
  *unreliably* right is worse, because its benchmark scores are not
  reproducible and a fix cannot be validated by observation.

We should not ship a lifetime whose outcome is decided by scheduler timing,
whichever outcome we prefer.

## Three documents, two of them wrong

The disagreement is not subtle, and it is worth recording exactly who says
what, because the model read one of them and believed it.

- **`docs/SPEC.md:1902-1912`** — accurate on the designed intent: no idle
  lease, no backstop, "its bound is legibility, not time," dies by `cancel`,
  `/clear`, or with the process. Correct, modulo the race above.
- **`core/src/types/shell/workers.rs:194`** — `"none — durable; dies by cancel,
  /clear, or process exit"`. Correct, and this is what `explain service`
  surfaces to the model.
- **`exarch/data/ral.md:193`** — *"For work that is meant to run indefinitely,
  use `service`"*. **Wrong**, and it is the model-facing document.

The instructive part is that the model *did* see the accurate string. The
transcript shows it running `explain service` and receiving "Dies only by
`cancel` through its handle, /clear, or process exit" verbatim — and then using
`service` for a server that had to outlive the process. Accuracy at the
definition site did not survive contact with a promise at the introduction
site. Whatever replaces `ral.md:193` must state the *consequence*, not the
mechanism: a grader, a test, or a human that looks after you exit will find
nothing. Compare mini's system prompt, which primes exactly this — *"Directory
or environment variable changes are not persistent. Every action is executed in
a new subshell."*

## The blind spot: exarch verifies from the wrong side of the boundary

`REPORT.md` §4.3 in the job directory states the deepest consequence, and it
generalises past this bug:

> *exarch checks its work from inside the process that owns the service. On
> kv-store-grpc the reply-verification nudge spent a whole round-trip
> confirming the server was healthy — and it was, because the process holding
> it open was the one asking. Every probe exarch can make is on the wrong side
> of the process boundary that kills the server.*

No amount of diligence closes this. The agent did everything right — started
the server, probed it, got a correct answer — and the probe was structurally
incapable of detecting the failure, because the prerequisite for making the
probe is the very thing whose absence would fail it. **A capability the agent
cannot test is one the runtime must guarantee.** This is an argument for
building `detach` properly rather than for teaching the model to be more
careful; there is no "more careful" available from inside the process.

## The vocabulary test: a verb is earned when the type changes

Adding a fifth concurrency verb needs a defence, because the existing four are
already suspiciously close together — and `docs/SPEC.md:1741` says so in as
many words:

> *`par`, `spawn`, `watch`, and `service` all produce the same kind of
> `Handle`.*

Four names, one type. What varies between them is a record of policy — where
output goes, whether a lease is armed, whether the body is `audit`-wrapped —
encoded into identifiers. On its face that is an argument for collapsing them
into options on one primitive, and it is *not* the argument this page makes.

**The test used here: a verb is earned when the type changes, not when the
policy changes.** `detach` passes it unambiguously. It does not return a
`Handle`; none of `await`/`poll`/`race`/`cancel` apply to it; its receipt is
data rather than a live reference. Whatever one concludes about the other
four, `detach` is a different construct and not a dial.

The converse — collapsing `watch` and `service` into `spawn` options — is
**rejected**, on a principle this wiki already holds. Per
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]], a builtin a host
cannot run should be *absent* from it, not present-but-vetoed: exarch
genuinely lacks `watch`, so naming it is an ordinary unknown-command error
rather than a permission denial, and the ral hosts genuinely lack `service`
because they grant no lease and every spawn there is already durable
(`docs/SPEC.md:1908-1912`). Options cannot be absent — only vetoed. The
verb-per-affordance shape buys legibility of *impossibility*, which is worth
more on a model-facing surface than economy of names.

So the count is not the defect. The defect is that **the names do not say what
axis they vary.** `spawn` versus `service` gives no hint the axis is time;
`watch` gives no hint the axis is where bytes go; and nothing at all announces
that a process boundary exists. Read as a set, with each verb labelled by its
axis:

| verb | you own it? | axis it varies | returns |
|---|---|---|---|
| `spawn` | yes | — (the primitive) | `Handle α` |
| `watch` | yes | where output goes — streamed, not buffered | `Handle α` |
| `service` | yes | time — no lease, bounded by legibility instead | `Handle α` |
| **`detach`** | **no** | **ownership — handed to the OS** | **receipt** |

`detach` is preferred over `daemon`, the other candidate
[[decisions/260617_long-running-work|long-running-work]] floated. `daemon`
names a *kind of program*, which invites the reader to ask whether their thing
is daemon-shaped; `detach` names the *act*, and the act is the entire content
of the decision. (That ADR rejected `daemon` for Regime 1 precisely because it
"connotes *survives process exit*" — a connotation correct for this regime, but
correctness about the effect is worth less than directness about the cause.)
`disown` remains unavailable: it names REPL job control (`ral/src/jobs.rs`,
[[map/repl/jobs|repl/jobs]]).

**The one sentence that must appear wherever these four are introduced
together:** the first three die when exarch dies; the fourth does not. That
fact is currently implicit in a lease class, a spec clause, and a doc string
that says "process exit" without saying which process or why a caller would
care — which is precisely how it failed to reach a model that had read the
accurate string minutes earlier.

## Decided

- **`detach` is a distinct verb, not a mode of `service`.** The rejection in
  [[decisions/260617_long-running-work|long-running-work]] stands and is
  strengthened by everything above: the two regimes have different handle types
  (`Handle` vs none), different observability (`poll`/`await` vs pid and exit
  status), different lifetimes, and essentially no shared code. `service` keeps
  its meaning — durable *within* the session, `Handle`-bearing, cancellable,
  listable, dead with the process.

- **Birth, not promote** — carried forward unchanged. You cannot promote an
  in-process thread into a surviving process, so Regime 2 independently
  re-confirms the rule.

- **The mechanism is double-fork, not cancel-scope evasion.** Two routes were
  identified to escape the chain above; they are not equivalent.

  - **(a) Double-fork.** The intermediate child exits immediately; the real
    server is reparented to pid 1 and its pgid is never observed by
    `RunningChild`. stdio is redirected to files at birth, so no pump pipe
    exists.
  - **(b) Register the worker under a cancel scope that is not a descendant of
    the session's `DurableRoot` (`core/src/types/shell/inherit.rs:304`),** so
    `cancel_all` (`core/src/types/shell/workers.rs:492-497`) cannot reach it.

  **(a) is chosen.** (b) defeats the signal but leaves the child holding a pipe
  whose read end closes when exarch exits — a quiet death by `EPIPE`/`SIGPIPE`
  for anything that logs, which is most servers. It would produce a capability
  that works in testing (where the server is quiet) and fails in production
  (where it writes an access log): the worst possible failure distribution. (a)
  fixes the signal and the pipe together, because redirecting stdio to files is
  part of the same act.

  The evidence that the pipe hazard is real and *separate* from the signal
  hazard: the gRPC server writes nothing to stdout or stderr ever, so it cannot
  have died by `EPIPE` — only a signal explains its death. Both mechanisms
  exist; (b) addresses one.

- **No `Handle`, and no pretence of one.** A surviving process is not a ral
  thread. `poll`/`await`/`race`/`cancel` do not apply to it, and giving it a
  `Handle` shape that silently means something different under the same
  eliminators would be a worse lie than `ral.md:193`. The receipt is data: an
  id, the pid, the paths its output was redirected to.

- **`exarch/data/ral.md:193` is corrected regardless of whether `detach`
  ships**, and corrected in terms of consequence rather than mechanism. This is
  the one item with no dependency on anything else here.

## Alternatives considered

- **Bless the accident: declare survives-exit a feature and rewrite
  `docs/SPEC.md:1907`.** Rejected twice over. First, on the evidence it isn't
  even true — the servers died. Second, and more durable: even if the race
  usually went the other way, ratifying it would bless a lifetime nobody
  designed, arriving with no pid, no readable output, no reconnect, and no
  reaping, under a verb whose own doc comment says "dead with `/clear` or the
  process." A behaviour you cannot explain is not a feature merely because it
  is convenient this week.

- **Make `service` itself survive.** Rejected: it fuses the two regimes the
  original ADR separated on the grounds that they share no implementation, and
  it would hand every existing `service` call site a new lifetime it did not
  ask for. It also strands the `Handle`: `await`-ing a process you can no
  longer see is not a coherent operation.

- **Add a synchronous shutdown sweep so the kill is designed rather than
  raced.** *Not rejected — deferred and reframed.* This was proposed while an
  earlier, incorrect trace suggested children were being orphaned, and it is
  the right response to the race described above, since it makes the documented
  behaviour true by construction on every exit path. The shape already exists
  in the repo: `ral/src/jobs.rs:308-346` (SIGTERM+SIGCONT, 5 s grace, SIGKILL,
  reap to `ECHILD`) plus the user-facing `survivor_warning` at
  `ral/src/repl/host_handlers.rs:111-126`. It must sweep a **pgid ledger**, not
  `kill(-pgid)` on exarch's own group, since the `setsid` is load-bearing and
  stays. It cannot cover the third-signal path
  (`core/src/process/signal/unix.rs:52-57`, `libc::_exit(128+sig)`), which is
  best-effort by construction. Sequencing note: this and `detach` are
  complements, not substitutes — the sweep makes "dies with exarch"
  deterministic, which is exactly what makes an explicit opt-out meaningful.

- **Tell the model to use `nohup … &` from inside a ral block.** Rejected on
  two independent grounds. Mechanically, `nohup` only sets SIGHUP to `SIG_IGN`;
  it changes no process group and starts no session, so it does not defend
  against `kill(-pgid, SIGTERM)` and could not defend against the subsequent
  SIGKILL even if it wanted to. It is the wrong tool by a wide margin, and the
  folklore around it obscures that. Conceptually, routing the model back into
  hand-rolled shell process management is the precise class of footgun ral
  exists to remove.

- **A `detached:` or `survives:` knob on `service`.** Rejected by the same
  reasoning [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
  used to reject `timeout:` on `spawn`: one primitive spanning both lifetimes is
  two implementations behind one name, and the caller reading the call site
  cannot tell which they got.

## Open questions

- **What `detach`'s receipt contains, and whether it is a value or a ledger row.** The
  minimum is an id, a pid, and the stdout/stderr paths. Whether reconnection is
  `service-handle`-shaped (a host-owned pinned row, per the amendment recorded
  in [[decisions/260617_long-running-work|long-running-work]]'s supersession
  note) or a plain returned record is undecided. Note the constraint from
  [[decisions/260629_agent-binding-reaping|agent-binding-reaping]]: whatever it
  is must survive compaction, since the model loses binding names.

- **Reaping and the zombie question.** A double-forked process is reparented to
  pid 1, which reaps it — so exit status is *not* recoverable by `waitpid` from
  exarch. If exit status matters, the intermediate must write it somewhere, or
  the design accepts "last known state" from the redirected files. This is a
  real semantic loss relative to `service` and should be stated in the verb's
  own documentation rather than discovered.

- **Interaction with the sandbox.** Under a granted base,
  `core/src/sandbox/linux.rs:64` applies bwrap's `--die-with-parent`, which
  kills the whole envelope on exarch's death regardless of double-forking.
  `detach` under a sandbox is therefore either impossible or requires escaping
  the envelope — which is a capability question, not a lifetime one, and
  probably wants a [[design/grant|grant]] authority of its own. **Until this is
  settled, `detach` is a `--base dangerous` capability only.** This deserves
  its own ADR.

- **Cap accounting.** A `service` counts against `LIVE_WORKER_CAP`
  (`exarch/src/shell_eval.rs:322`, admission at
  `core/src/builtins/concurrency.rs:220-228`). A process that outlives the
  session cannot meaningfully occupy a seat in it — but neither should birthing
  a hundred of them be free.

- **Confirming the race empirically.** One run settles which side wins and how
  reliably: `RAL_DBG=wait` emits `cancel-fired name=… cause=Explicit` from
  `core/src/runtime/command/child.rs:431-438` if and only if the worker thread
  reached the kill. Cheap, and it converts the central inference of this page
  into an observation.

- **Whether the benchmark should be re-run.** `REPORT.md` labels 5/5 as a
  counterfactual and recommends a re-run. Both zeros are attributable to this
  gap, so the 0.6 understates exarch on a harness-capability axis rather than a
  reasoning one — but the counterfactual should not be quoted as a score.

## See also

[[decisions/260617_long-running-work|long-running-work]] (the page this
revives; Regime 1/Regime 2, birth-not-promote, and the recommendation to defer
Regime 2 "unless a concrete need appears" — this is that need),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the detached-root model, the death-clock, and the rejected `timeout:` knob),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (core supplies the
mechanism, the host registers the affordance — the pattern `detach` should
reuse),
[[decisions/260629_agent-binding-reaping|agent-binding-reaping]] (why a
rediscoverable id matters more than a binding),
[[internals/output-capture-and-detachment|output-capture-and-detachment]] (the
narrative this amends: `spawn`/`watch`/`service` and the registry),
[[map/repl/jobs|repl/jobs]] (the REPL's sweep and survivor warning, the shape a
shutdown sweep should copy),
[[design/grant|grant]] (where the sandbox question lands), and `docs/SPEC.md`
§13.4.
