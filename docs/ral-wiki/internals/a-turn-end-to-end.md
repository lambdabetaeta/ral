---
verified_at_commit: 7ba500b
verified_at_date: 2026-06-17
anchors: [eval_top_level, Mobile, run_turn, run_shell, session_schemes, compile_and_typecheck]
---

# A turn, end to end

Both binaries drive the same engine; a *turn* is one top-level evaluation against
a persistent `Shell`. The two frontends differ only in who produces the source
and whether a capability frame is pushed.

**A ral REPL line runs four stages over the session's persistent `Shell`:**

- The [[map/repl/frontend|frontend]] reads a line.
- The [[map/repl/loop|`Session`]] threads its persistent `Shell` into
  `compile_and_typecheck`, seeded from the live session (`shell.session_schemes()`,
  the one name→scheme seed — the prelude's schemes ride scope[0] from boot, so
  there is no separate prelude seed) ([[internals/compilation-ladder|the ladder]];
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
- `eval_top_level` runs the *annotated* typed IR the checker returned
  ([[internals/evaluator-machine|the machine]]).
- The post-run `Mobile` installs back onto the session's shell, so a `let` or
  `cd` on this line is visible on the next; the returned value is printed.

Errors and `exit` install state too — the turn is a resume point regardless of
outcome.

**An exarch tool call is the same turn under a host-pushed grant frame.** The
provider streams an assistant reply containing a `shell` tool call;
[[map/exarch/shell-eval|`run_shell`]] compiles the model's source seeded from the
same live session (`shell.session_schemes()`), then pushes the session
`Capabilities` with
`Shell::with_capabilities` **before** `eval_top_level`. That pushed frame *is* the
sandbox — ral's [[design/grant|grant]], not a source-level `grant { … }` the model
could escape. Output is teed:

- one branch is the full, model-visible buffer;
- one is a live head/tail digest to the terminal.

The post-run `Mobile` installs as before, so the agent's `cd` and `let` persist
across tool calls. The [[map/exarch/session|session]] loop repeats provider
round-trips until the model emits no tool call, with auto-compaction bounding the
transcript. However a turn ends — completion, cancellation, or a surfaced
provider error — it returns the session ready for the next prompt
([[invariants/turn-ends-ready|turn-ends-ready]]), the agent-side echo of the ral
turn's resume point above.

**The shared spine.** Strip the frontends away and both are `eval_top_level`
over one persistent `Shell`, installing `Mobile` on every outcome. The human and
the model are interchangeable sources of top-level turns; the only structural
addition on the agent side is the host-pushed grant frame. This is why exarch
needs no runtime of its own — see [[design/exarch-architecture|exarch-architecture]].

See also [[internals/compilation-ladder|compilation-ladder]],
[[internals/evaluator-machine|evaluator-machine]]; maps [[map/repl|repl]],
[[map/exarch|exarch]].
