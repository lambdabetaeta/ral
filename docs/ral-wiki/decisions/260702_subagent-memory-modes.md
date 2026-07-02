---
status: active
---

# Subagent memory modes

**Sub-agent spawning has two names because it has two different model-memory
contracts: `agraphos` is tabula rasa; `anamnesis` remembers the parent context
and appends a fresh prompt at the end.**

Both tools keep the same operational skeleton:

- a child `Agent` is born under the spawning agent's `parent` edge;
- the shell is still a value-snapshot via `Shell::fork_session`;
- authority is still `parent ⊓ resolve_base(permissions)`;
- the child runs detached through the same `drive` loop and returns later through
  the parent's inbox.

The distinction is only the child's provider transcript:

- **`agraphos`** starts with no parent conversation. It receives only the launch
  prompt as its first user turn. Use it when blank context is a feature.
- **`anamnesis`** imports the parent's model-visible context, reuses the parent's
  current provider selection, and appends the tool call's `prompt` as the fresh
  final user prompt. Use it when the child should stand inside the current
  conversation and hit provider prompt caches.

Because a tool call is dispatched while the parent may be waiting for that very
tool result, `anamnesis` must not inherit a dangling assistant tool-call frame.
`AgentLog::inherited_context_messages` drops the unanswered assistant tool-call
event when the parent is in `AwaitingToolResults`, so the child forks the request
context rather than an invalid protocol prefix.

This names the two costs honestly. A remembered sub-agent spends less rebuilding
context but carries the parent's assumptions; a blank sub-agent pays the full
context boot but gives a genuine fresh read.

## Links

[[design/agents|agents]], [[map/exarch/agent|agent]],
[[map/exarch/tools|tools]], [[map/exarch/provider|provider]],
[[decisions/260617_async-agent-tool|async-agent-tool]],
[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]].
