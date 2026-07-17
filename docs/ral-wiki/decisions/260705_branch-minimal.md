---
status: active
---

# `/branch` (minimal): a conversing mnemon child

**A branch is an ordinary parented registry child that *converses*.** `/branch
[prompt]` forks the trunk's conversation into a new tab — mnemon-style context
inheritance — but withholds `reply`, so the child parks `Held` for the human
while its registry entry lives and quiesces the moment the entry is gone. No
`Station`, no parent-less registration, no per-agent generations: four small
mechanisms and one command, all resting on a single enabling concession.

This is the *minimal cut*. The full plan lets branches survive the trunk's
`/clear` and drop out of the trunk model's `agents` view; that machinery existed
almost entirely to buy those two properties. Give them up and the branch collapses
to a plain child.

## Conversing is derived from the tool view

`returns()` reads whether the agent holds `reply` (the `Gate::Returns` tool), not
its `parent`/`interactive` position. The tool view is the **single source of
truth**, so parking, the reply-nudge, and the advertised tools cannot disagree: an
agent built without `reply` converses, full stop, wherever it sits in the tree
([[design/agents|agents]]). This is what lets a conversing agent exist that is not
the trunk — a branch is interactive, `reply`-withheld, and parented. `park_mode`
reads `interactive && !returns()`; a conversing agent whose entry has been reaped
reads `!is_live` and quiesces rather than parking `Held` on a dead entry (a
zombie).

## The enabling decision: `/clear` on the trunk clears everything, branches included

That single concession keeps a branch a plain registry child, and it dissolves —
for free — the two problems that made a naive "mnemon minus `reply`" unsound:

- **The `/clear` zombie.** `/clear` runs `clear_subtree(trunk)`, which removes the
  branch entry as an ordinary descendant; within one poll the branch's park reads
  `is_live == false`, quiesces, and its tab retires. Only the trunk can `/clear`,
  so the global clear generation stays correct.
- **The 1-hour ceiling.** A branch registers with the ceiling **off**: a
  conversation must not lose a turn at the hour mark, and a user-seated turn is
  human-typed, not model recursion the ceiling exists to bound. `register` takes an
  explicit `ceiling: bool` — sub-agents pass `true`, a branch `false`.

## Empty settle epilogue: no branch death touches the trunk

A branch pushes **no** `AgentResult` upward. Its result is never wanted, so the
settle epilogue is gated empty: `Kind::Died` still fires, but the
render-reply / commitment / `parent_mailbox.push` / `settle` block runs only for a
returning child. This is why no branch death — `/clear`, `/close`, or process exit
— ever pollutes the trunk's context: not because a late result is caught by
generation admission, but because the branch never posts one. The sub-versus-branch
split is derived once, at the spawn site, from `child.returns()`: the same tool-view
fact drives the ceiling knob, the tab kind, and the empty epilogue, so none can
drift from the child's actual tools.

## The command surface

- **`/branch [prompt]`** forks the trunk's conversation: mnemon context
  inheritance, the trunk's capabilities **verbatim** (user-seated — no `permissions`
  argument, no narrowing), an own provider handle seeded from the trunk's current
  model (cache hits), and fuel = creator − 1 so a branch may itself spawn. A bare
  `/branch` parks and waits on its tab; `/branch <prompt>` seeds the prompt as the
  fresh final turn. It is handled on the trunk at the turn boundary, host-side
  like the rest of the slash-command surface, and carries **no fuel gate**
  (human-typed).
- **`/close`** on a branch tab is the *one* command admitted off the trunk — a pure
  frontend registry op (`remove_subtree`: `cancel_entry` then remove `{focused} ∪
  descendants`), never routed to a worker, so it needs no per-tab control. It errors
  on the trunk and on a plain sub-agent tab; the frontend tells a branch apart by a
  branch-tab set on `Tabs`, seeded when the branch's `Born` arrives.

Steering is otherwise the ordinary registry-mailbox path — typed lines are the
branch's next turns. Any other recognised worker command typed on a branch tab is
refused ("not available on this tab") by the unified parser, never mailed to the
model.

## Deliberately accepted warts

Named here so nobody rediscovers them as bugs:

- `/quit` on the trunk kills branches with the process (their `user.log`s still
  flush). It has a named fix in the full plan if branches earn the upgrade.
- Ctrl-C (and Esc) no longer reach a branch's turns from the trunk at all —
  [[decisions/260705_cancel-per-tab|cancel-per-tab]] made both cancel keys per-tab,
  so a branch turn is interruptible only from the branch's own tab.
- The trunk's model still sees a branch in `agents` and may `message` it (harmless,
  occasionally useful); `agent_cancel` can cancel its *turn* but cannot kill it —
  `Held` parking ignores the token.

## See also

[[decisions/260705_cancel-per-tab|cancel-per-tab]] (per-tab cancellation, which
ships first and is why Ctrl-C no longer reaches a branch),
[[design/agents|agents]] (the tool-view derivation and the uniform node),
[[decisions/260702_subagent-memory-modes|subagent-memory-modes]] (the mnemon
inheritance a branch reuses). Maps: [[map/exarch/agent|agent]],
[[map/exarch/tools|tools]], [[map/exarch/frontend|frontend]].
