# Exarch's modules form a DAG

**Exarch's top-level modules form a directed acyclic graph: a `crate::` reference from a module to one that already reaches it, outside tests and comments, is a build break.**

The layers, leaves first: a module sits one above its deepest dependency, so
modules on one line are unordered (see
[[design/exarch-architecture|exarch-architecture]] and [[map/exarch|exarch]]):

- `app`, `card`, `clock`, `latch`, `library`, `site`, `skill`
- `provider`
- `config`, `policy`, `record`
- `bus`, `net_policy`, `prompt`, `wallet`
- `schedule`
- `enquiry`
- `shell_eval`
- `agent`
- `port`, `signals`
- `boot`, `host`, `tui`
- `headless`
- `cli`, `lib`

Each boundary says what the lower layer is spared:

- `provider` knows no `record`: a provider speaks the wire and nothing of the session's durable log.
- `record` knows no `bus`: it publishes through its `Publish` trait, never to a concrete bus.
- `bus` knows no `agent`: it owns `AgentMailbox` / `AgentAddress` and uses
  ral scopes for life/exchange cancellation. `schedule` and `shell_eval`
  reach that mailbox without depending on its agent owner.
- `agent` knows no `port` or `host`: `agent` and `bus` sit below `port`,
  which sits below `host`, `tui` and `headless`. The host launches and joins
  roots; the port addresses them without retaining their launch.
- Frontends reach agents through `port`: `tui` names neither `agent`, `bus`
  nor `host`; `headless` names neither `agent` nor `bus` (it may observe its
  host's feed). Outside the crate, only `host` and `port` reach agents
  ([[decisions/261029_a-frontend-holds-a-port|a-frontend-holds-a-port]]).
- `shell_eval` knows no `agent`: the desk is reached through `ral_core::carrier::Host`.
- `prompt` and `enquiry` know no `agent`: both are read by the agent, never the converse.

Pinned by `exarch/tests/layering.rs::{exarch_modules_form_a_dag,
frontends_reach_agents_through_the_port}`. The graph is every production
`crate::` reference between top-level modules, `main.rs` counting as `lib`; the
first test rejects a cycle and each ordered pair of `LAYERED`
(`provider < record`, `record < bus`, `bus < agent`, `agent < port`,
`port < host`, `port < tui` and the rest), and the layers above are the graph as
it stands, not an order the test fixes beyond those pairs.
`control_commits_belong_to_record_and_bus` keeps control commits in `record/`
and `bus/`.
