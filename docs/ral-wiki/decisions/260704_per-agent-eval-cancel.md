---
status: active
---

# Per-agent eval-layer cancel; only the trunk publishes the signal slots

> The publication half is superseded by
> [[decisions/260726_cancel-is-a-join|cancel-is-a-join]]: a handler contributes
> a cause to two process-lifetime cells that facing scopes fold, so "only the
> trunk publishes" becomes "only the trunk's session is minted facing" — the
> rule is carried by construction rather than by a flag. The eval-layer cascade
> below is untouched.

An Esc or `agent_cancel` set exarch `Token`s and nothing else. Two structural
gaps made the cancel path slow and, under a running fleet, unsound:

- **The token never reached a running eval.** `run_shell` takes no token: it
  dispatches the turn and blocks on `transport.events().recv()` until the
  engine reports. The registry cascade set only `Token`s, which the drive loop
  reads *between* steps — so a cancelled sub-agent mid-`ral`-tool ground on to
  its `timeout_secs` wall (default 60 s, model-raisable) before noticing, its
  subprocesses alive the whole time. `/clear` and a settling `reply` reaped
  entries the same way, leaving orphan evals burning.
- **Every session published the process-global signal slots.** `TurnGuard`
  published `FOREGROUND_SCOPE`/`DURABLE_ROOT_SCOPE` unconditionally, but the
  slots' save/restore-previous discipline is sound only for LIFO publication on
  one thread. Sub-agents dispatch turns concurrently on their own threads, so
  the trunk's Esc (`request_foreground_cancel`) hit whichever session
  dispatched *last* — possibly a sub-agent's eval, leaving the trunk's turn
  running — and cross-thread restore order could park a pointer to a freed
  `ScopeNode` flag in the slot: a latent use-after-free under the very
  interleaving that made Esc feel dead.

## The decision

Two moves, one invariant each.

**Only the signal-facing session publishes the slots.**
`SessionState::publishes_signal_slots` marks it; `Shell::new` sets it,
`Shell::fork_session` — the sub-agent seam, which already renounces terminal
authority — clears it, and `TurnGuard::install` publishes only when it is set.
Publication is again LIFO on a single thread (the safety prerequisite the slot
SAFETY comments state), and a signal can only ever target the primary
session's turn. This is the rule exarch's own token slot already enforced
("only the trunk publishes"), now honoured by ral's slots too.

**The registry cascade cancels both layers.** `Shell::cancel_handle` returns a
clonable `DurableRoot`; each parented `AgentRegistry` entry carries one
(`eval_root: Option<DurableRoot>`, captured at registration), and both cascade
primitives (`cancel`, `remove_descendants`) cancel it with
`CancelCause::Explicit` alongside the `Token`. The root cancel unwinds an
in-flight eval at the evaluator's existing poll points — the trampoline,
`await`'s 1 ms sweep, the child-wait loop's 5–100 ms backoff with
cause-directed group teardown — so Esc-to-stop is bounded by poll cadence
(~100 ms; ~600 ms with an external child's SIGTERM grace), not by
`timeout_secs`. The trunk registers no eval-root: its session outlives any
cancel, and latching its durable root would kill every future turn and
detached worker; its turn-level cancel rides the published foreground slot.

## Where

- **`core/src/types/shell/mod.rs` / `init.rs` / `inherit.rs`** —
  `publishes_signal_slots` on `SessionState`; set at `Shell::new`, cleared by
  `fork_session`.
- **`core/src/turn.rs`** — `TurnGuard` holds `Option`al slot guards, published
  only for the signal-facing session.
- **`core/src/types/shell/host.rs`** — `Shell::cancel_handle`, the host verb
  for a non-signal-facing session's stop lever.
- **`exarch/src/agent_registry.rs`** — `eval_root` on `Entry`; `cancel_entry`
  cancels token + root for both cascade primitives.
- **`exarch/src/agent.rs` / `tools/agent.rs`** — `Agent::eval_root` reads the
  handle off the agent's shell; `register_self` and `spawn_async` thread it
  into registration.

## Covered

- `turn::tests::forked_session_turn_does_not_publish_signal_slots` — a
  foreground-cancel request cannot reach a forked session's turn; its
  `cancel_handle` produces the same 130 unwind the slot gives the trunk.
- `agent_registry::tests::cancel_sets_the_token_and_list_reports_the_subtree`
  and `clear_subtree_cancels_descendant_eval_roots` — both cascade primitives
  cancel both layers.
- `agent::tests::reply_cancels_live_descendants` — the reply reap cancels an
  abandoned child's eval layer.
- `shell_eval::tests::root_cancel_unwinds_inflight_run_shell` — end to end: a
  forked session blocked in `run_shell` on a 30 s sleep under a 30 s budget
  returns in well under a second when its handle is cancelled, and the spawned
  process tree is reaped.

## The hard rule

At most one session per process publishes the signal slots — the signal-facing
one. Every other session is stopped through its own `DurableRoot` handle, and
any host cascade that cancels an agent must cancel **both** layers: the
cooperative token its drive loop polls, and the session root its eval polls.
A token without the root handle is a request the eval cannot hear.

See also [[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] (the
token layer this completes), [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
(the delivery half), [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]
(the root/foreground split the handle leans on), [[internals/cancellation|cancellation]],
and [[map/exarch/agent|agent]].
