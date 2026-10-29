# Exarch's modules form a DAG

**Exarch's top-level modules form a directed acyclic graph: a `crate::` reference from a module to one that already reaches it, outside tests and comments, is a build break.**

The layers, leaves first (see [[design/exarch-architecture|exarch-architecture]] and [[map/exarch|exarch]]):

- `clock`, `latch`, `card`, `app`, `skill`, `library`
- `provider`
- `record`
- `bus`
- `schedule`
- `enquiry`
- `shell_eval`
- `agent`
- `boot`
- `port`
- `host`
- `tui`, `headless`
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
frontends_reach_agents_through_the_port}`. Its ordered pairs include
`agent < port`, `bus < port`, `port < host`, `port < tui` and `port < headless`.
