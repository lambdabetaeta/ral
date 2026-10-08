# Exarch's modules form a DAG

**Exarch's top-level modules form a directed acyclic graph: a `crate::` reference from a module to one that already reaches it, outside tests and comments, is a build break.**

The layers, leaves first (see [[design/exarch-architecture|exarch-architecture]] and [[map/exarch|exarch]]):

- `clock`, `latch`, `cancel`, `card`, `app`, `skill`, `library`
- `provider`
- `record`
- `bus`
- `schedule`
- `enquiry`
- `shell_eval`
- `agent`
- `boot`
- `tui`, `headless`
- `cli`, `lib`

Each boundary says what the lower layer is spared:

- `provider` knows no `record`: a provider speaks the wire and nothing of the session's durable log.
- `record` knows no `bus`: it publishes through its `Publish` trait, never to a concrete bus.
- `bus` knows no `agent`: the inbox's `Stamp` and `cancel::Token` suffice.
- `shell_eval` knows no `agent`: the desk is reached through `ral_core::carrier::Host`.
- `prompt` and `enquiry` know no `agent`: both are read by the agent, never the converse.

Pinned by `exarch/tests/layering.rs`.
