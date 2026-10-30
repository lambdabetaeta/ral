---
status: active
generated_at_commit: fc0c9fbb
generated_at_date: 2026-10-30
verified_at_commit: fc0c9fbb
verified_at_date: 2026-10-30
anchors: [Port, AgentRow, Registry, Host, Reader, Selected, Agent::switch_model, Tabs::settled, Leaving]
amends: [261029_an-agent-is-a-thread]
---

# A frontend holds a port

**A frontend addresses agents through a `Port`; a `Host` owns their launch,
feed and joined end.** This is the public boundary between exarch and its
clients, including Synod.

## The laws

- **P1 — One reach.** Frontends name agents by `AgentId` and act only through
  `Port`. The TUI names neither `agent`, `bus` nor `host`; headless names
  neither `agent` nor `bus`. The layering test pins this reach rule.
- **P2 — Folded, not copied.** An agent owns its immutable client in
  `Body.client`. The port reads cell `Status`, including the mail's debt
  and `Model::selected()`, exposed as `AgentRow.selected`, never the client.
- **P3 — Frontends never write a log.** `Born.reader` permits TUI catch-up,
  not append. `Emitter` and `AgentLog` are crate-private; model switches are
  recorded by the agent, not its frontend.
- **P4 — A port never retains a launch.** It holds `Arc<Registry>` and a
  usage meter, not `Fleet` or `Launch`. A port retained across `Host::end`
  cannot keep Synod's engine factory shared. A weak fleet reference would
  still retain the launch during an upgraded call racing with end.
- **P5 — A frontend's end is observed.** After draining to empty, the TUI
  may finish when every known root tab has received `Died`. All initial
  births precede `tui::run`; a later root's birth precedes its creator's
  death; a helper's death precedes its root's death because teardown joins
  helpers first. Thus no producer can remain unseen after that drain.

## Boundary and effects

- `host` exports construction, admission and engine types; `port` exports
  operations and signal/command types. `agent`, `bus`, `boot`, `cli`,
  `enquiry`, `headless`, `library`, `schedule`, `shell_eval`, `skill` and
  `tui` are crate-private. `app`, `card`, `clock`, `config`, `net_policy`,
  `policy`, `prompt`, `provider`, `record` and `wallet` remain public launch
  support: hiding them would redesign Synod's construction, not isolate agents.
- `Command::Model(Pick)` asks the owner to build. Success records
  `Control::ModelChanged { selected }` **before** replacing the client.
  The bureau's immutable `Selected` stamp, not the requested `Pick`, is
  installed in the cell's model fold. Plan-billed accounts discard sampling.
  A failed build records an error but changes neither client nor model;
  failed recording ends life and prevents the switch.
- The TUI persists a selection only from a live commit's `ModelChanged`,
  using that record's account, label, model and `via`. Asking saves nothing;
  replay/catch-up does not change project selection. Login/provider notes
  are view chrome and do not appear in record-backed exports.
- A switch takes effect at the next request, even in the same deliberation.
  An in-flight request retains its client; the next reads `Body.client`
  afresh. Helpers using `enquiry::Selection::Inherit` inherit that client.
- `Port::end(agent)` ends that agent and its descendants; their owners still
  join them. There is no port operation to end a fleet. The host owns its
  single feed and fleet-wide end; the port offers no feed subscription.
- `/quit` returns from the TUI without ending agents. `tui::run` returns
  `Leaving` after restoring the terminal; `lib::run` calls `Host::end`, then
  prints `Leaving::report` with final run usage from its retained port plus
  recalled usage. Exchanges cut short by quitting are included in that spend.

## Amendment and limits

This amends [[261029_an-agent-is-a-thread]]: the immutable client is owner-held,
while recorded selection belongs to the cell's `Model`, and P5 replaces
A8's `Host::is_finished` completion criterion. The host still joins and collects failures.

The wire, broadcast subscriptions, `Port::since`/history and idempotent
submission are not implemented here. Resume does not restore the last
`ModelChanged` as its client. A switch uses the launch's CLI token cap
wherever the new account admits it.

## Amendment — 2026-10-30: the port reads the folds

[[design/agents|agents]] amends P2 and switching above:
`AgentRow.selected` replaces a published `Pick`; model, account and tuning
come from the cell, and each agent learns its own window. The TUI labels a
currently available account dynamically, falling back to `selected.label`
when it cannot find it. Only a live `ModelChanged` persists a next-launch
choice; birth and reopen do not.

## Where

- `exarch/src/{lib,host,port}.rs`, `agent/fleet.rs`: public boundary, registry
  ownership, feed, joins and report ordering.
- `exarch/src/agent/{command,attend,deliberate}.rs`, `bus/mailbox.rs`,
  `provider/bureau.rs`: normalized selection, recorded switching and attempts.
- `exarch/src/record/{seam,session}.rs`, `tui/{app,model_picker,scrollback,tabs,tui_loop}.rs`:
  read-only catch-up, record-driven persistence and observed completion.
- `synod/src/assistant/{guest,conversation}.rs`, `window/{state,conversation}.rs`:
  root ids, per-agent end and launch-independent inlets.
- `exarch/tests/layering.rs`: DAG and frontend reach checks.

[[map/exarch/frontend|frontend]], [[map/exarch/agent|agent]],
[[map/exarch|exarch]], [[map/synod|synod]],
[[invariants/exarch-is-a-dag|exarch-is-a-dag]].
