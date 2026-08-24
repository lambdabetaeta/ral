# exarch: an agent as a provider loop over one `ral` tool

**exarch is a small LLM coding agent that embeds [[map/core|ral-core]] behind a
deliberately thin architecture.** A model is given one `ral` tool, and every
tool call is evaluated as a ral top-level run against a persistent in-process
`Shell`. The `agents` and `fff` surface is provided by ral builtins inside that
call, not by additional provider tools. The agent loop is just provider
round-trips, repeated until the model emits no tool call
([[map/exarch/agent|agent]]):

- render the transcript;
- stream a reply;
- dispatch any tool calls;
- append the results.

**The sandbox is not invented; it is ral's.** Each run evaluates under a
profile's capabilities pushed onto ral's capability stack, so the safety
boundary *is* the [[design/grant|grant]] mechanism — authority attenuated by
intersection. There is no source-level `grant { … }` the model could escape: the
host installs the frame around the run, and external commands route through the
same OS sandbox ral uses. Capability composition (`base ∨ extend ⊓ restrict`)
and the bake-in profiles are [[map/exarch/policy|policy]].

**The session is one continuous evaluation context, not independent
shell-outs.** The top-level contract installs each run's post-run state onto the
shell, so `let`, `cd`, and env persist across tool calls. Three mechanisms keep
that context bounded and isolated:

- **persistence.** `let`, `cd`, and env carry across tool calls;
- **auto-compaction.** Long autonomous runs stay bounded — the history is
  summarised when it crosses a threshold, and a nudge policy decides whether to
  stop or loop with a synthetic next prompt;
- **sub-agent isolation.** A sub-agent forks a value-snapshot of the parent's
  shell context; its mutations do not propagate back, mirroring the subshell
  isolation of a [[design/pipelines|byte-pipeline stage]].

**Why this shape works.** The language already supplies what an agent needs, so
exarch adds only LLM transport and exchange orchestration:

- the abstraction — the [[design/cbpv|block]];
- the confinement — grant;
- the structural [[design/audit|audit tree]].

Crucially, the agent's "shell" is a real typed language, not string-splicing:
because data is never re-lexed, split, or globbed once captured, the collapse of
data / command name / re-lexable source that makes shell-driving agents fragile
simply does not arise.

See also [[design/grant|grant]], [[design/cbpv|cbpv]], [[design/audit|audit]],
[[design/hash-addressed-editing|hash-addressed-editing]].

**Realised in** [[internals/a-turn-end-to-end|a run, end to end]].

Code maps: [[map/exarch|exarch]] hub, [[map/exarch/agent|agent]],
[[map/exarch/shell-eval|shell-eval]], [[map/exarch/policy|policy]]. Human docs:
`exarch/README.md`, `exarch/PROFILES.md`.
