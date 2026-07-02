---
status: active
---

# `/discuss` is host orchestration over ordinary agents

**`/discuss <prompt>` starts one returning *chair* agent and lets ordinary
sub-agent protocol do the rest.** The command is a TUI slash command, not a new
agent kind and not a new inbox delivery mode.

- The focused agent handles `/discuss` at the normal `Control` boundary, where
  the drive thread owns the shell and may fork safely.
- The command spawns an `anamnesis` chair: it imports the focused
  model-visible context, receives the user's topic as its fresh final prompt,
  and holds the usual returning-agent `reply { result }` tool.
- The chair is instructed to spawn exactly one `agraphos` partner, ask it for an
  independent critique, wait for the partner's normal `reply`, then call its own
  `reply` with one `result` field.
- The first version deliberately does **not** use `message`, and does not make
  peer traffic look like human input. The discussion is a bounded two-agent
  fork/join: chair → partner → chair → parent.
- Authority is inherited verbatim for both discussion edges by using the
  existing `dangerous` base, which is the spawn lattice top: it narrows nothing
  but still cannot exceed the parent.

This keeps the interactive trunk out of the debate, preserves
[[design/agents|agents]]' uniform-node model, and avoids a special "discussion"
transport. If richer back-and-forth becomes necessary, it should be a new typed
protocol decision, not an accidental reinterpretation of marked `message`
turns.
