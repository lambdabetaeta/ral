---
status: fixed
---

# Redirects dropped on handler dispatch

When a command resolved to a handler — a runtime `alias` or a
`within [handlers:]` / `[handler:]` entry — its fd redirect targets were
evaluated but never installed, so `git log > out.txt` under a `git` alias
silently wrote nothing to the file. The handler dispatch arm did not wrap its
body in `with_redirects` the way the Env, Builtin, and External arms did — an
oversight from when handlers were added, not a deliberate semantics.

The fix (`270ca13`, 2026-05-27) wraps the handler arm in the same redirect frame
as the others, so a redirect on a handled command takes effect and any command
the handler forwards to inherits the frame:

- **Where** — `core/src/evaluator/command_call.rs`, the `Resolution::Handler` arm.
- **Covered** — `ral/tests/pipeline.rs` asserts all fd directions (stdout, stderr,
  stdin), both handler forms (per-name and catch-all), the value-returning mock
  case, and a pipeline-stage handler.

See also [[design/effects-handlers|effects-handlers]], [[map/core/evaluator|evaluator]].
