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
- The chair spawns exactly one `agraphos` discussant (titled after the topic) with
  `read-only` permissions, and gives it a prompt instructing it to engage via
  `message`.
- The chair sends the topic as a `message`; the discussant responds in kind.
  Each round, the chair exposes both a sharp challenge and its own position;
  the discussant must defend its view *and* critique the chair's.
- They trade `message` calls for at least 10 exchanges. The chair updates its
  position each round (conceding where hit, sharpening where it disagrees).
  Both stop when the debate has genuinely matured.
This keeps the interactive trunk out of the debate and preserves
[[design/agents|agents]]' uniform-node model: the discussant is one ordinary
returning agent, and the back-and-forth uses the existing `message` protocol
rather than a special peer channel.
