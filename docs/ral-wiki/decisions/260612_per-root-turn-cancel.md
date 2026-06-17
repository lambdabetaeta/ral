---
status: active
---

# A per-root-turn cancellation token for exarch

Cancellation was a single process-global `AtomicBool` cleared at the top of
*every* `Session::apply`. The deep review (260611 §8, recommendation A11)
named three defects in that shape:

- **X5** — `apply` began with `cancel::clear()`. A sub-agent runs the child's
  `apply` on a scoped thread; an Esc that landed after the `agent` dispatch
  began but before the child entered `apply` was wiped by the child's own
  clear, so the parent's checks, the streaming race, and the post-dispatch
  checks all read false. The clear belonged at the root turn only, not at every
  `apply`.
- **X8** — `/clear` rebuilds the shell through `boot_shell`, which calls
  `ral_core::process::install_handlers` and so reinstalls ral's bare handlers
  *over* exarch's chained handler. After `/clear`, an external SIGINT/SIGTERM
  no longer raised the cancel flag.
- The [[decisions/260523_background-tool-calls|background-tool-calls]] study
  named the global flag as the blocker for per-job cancel.

## The decision

Mint a cancellation **`Token`** once per *root* turn (`run_turn`) and thread it
explicitly down the call chain: `apply` → `dispatch` → `stage` → `Tool::dispatch`
→ a forked child's `apply`. A `Token` is an `Arc<AtomicBool>`; cloning shares
the flag, so a sub-agent handed a clone is cancelled the instant the root token
is — **one Esc cancels the whole tree**. `run_turn` is the only mint point
(sub-agents call `apply` directly, never `run_turn`), and *minting a fresh
token is itself the reset* — there is no `clear()` on the root path, and none
inside `apply`, so a sub-agent's `apply` can no longer erase a pending Esc (X5).

The signal handler cannot own a token by value, and it must be async-signal-safe
(no locking). So the root turn publishes its token's flag into a process-global
**`AtomicPtr` slot** — the lock-free analogue of an `ArcSwap` — that the handler
reads and sets with one atomic load plus one atomic store. `cancel::is_set`
reads the same slot, so the one call site with no token in scope — the
provider's mid-stream cancel race (`wait_for_cancel`, the streaming
`tokio::select!`) — observes exactly the cancellation the threaded `Token` does.
An RAII `RootGuard` owns the token's `Arc` for the slot's whole tenure and nulls
the slot on drop, so the published pointer is retired before the `Arc` it
borrows can free, however the turn ends.

`bootstrap::boot_shell` owns exarch's session-shell signal ceremony: it clears
any stale ral process interrupt before embedded library evaluation, installs
ral's handlers, and immediately re-`install`s the chained handler. `Session::clear`
rebuilds only through that constructor, so `/clear` inherits both the stale-
interrupt discard and the SIGINT chain survival (X8).

## Where

- **`exarch/src/bootstrap.rs`** — `boot_shell` is the session-shell constructor:
  clear stale ral interrupt, install ral handlers, re-chain exarch cancel, then
  load the exarch builtins/library.
- **`exarch/src/cancel.rs`** — `Token`, the `CURRENT` `AtomicPtr` slot,
  `mint_root` / `RootGuard`, `is_set` / `raise` over the slot, the chained
  signal handler.
- **`exarch/src/session.rs`** — `run_turn` mints the root token; `apply`,
  `dispatch`, `stage` thread it and check `token.is_cancelled()`; `cancelled`
  and `apply` no longer clear; root-session shell rebuilds call the constructor
  rather than repeating signal ceremony.
- **`exarch/src/tools.rs`** + `tools/agent.rs` — `Tool::dispatch` carries the
  token; the `agent` tool clones it into the child's `apply`.

## Covered

- `cancel::tests::fresh_mint_does_not_inherit_prior_cancel` — minting is the
  reset; a prior turn's cancel does not bleed into the next (X5).
- `cancel::tests::child_token_shares_parent_cancellation` — a child clone is
  cancelled with the parent.
- `cancel::tests::boot_shell_restores_the_chain_after_handler_clobber` — a
  clobber before the exarch constructor leaves a later interrupt still raising
  the token (X8).
- `session::tests::subagent_apply_honours_a_shared_cancelled_token` — a
  sub-agent's `apply`, driven on a cancelled shared token, short-circuits to
  `Cancelled` rather than running its scripted turn.

## The hard rule

Cancellation state is per *root* turn, minted at `run_turn` and shared down the
tree by `Token` clones. Never reset it inside `apply` (a sub-agent would erase a
pending Esc); the only reset is the next root mint. Any host operation that
rebuilds a session shell must use `bootstrap::boot_shell`; stale-interrupt
discard and handler re-chaining live there, not at each call site.

See also [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
(the delivery half — what an Esc *does* once it raises the token),
[[decisions/260504_hot-path-cancellation|hot-path-cancellation]] (ral's
per-subtree `CancelScope`, the in-evaluator analogue), [[map/exarch/frontend]],
[[map/exarch/session]], and [[invariants/turn-ends-ready]].
