---
status: active
---

# An authenticated confinement marker

**The marker that tells a ral process it is already OS-confined is trusted
only when it carries a capability token a genuine sandbox re-exec minted —
never on the mere presence of the inheritable `RAL_SANDBOX_ACTIVE` env
var.** The presence test was load-bearing for the *whole* OS layer, and
the OS layer is the sole enforcer of `net` and of bundled-coreutils
filesystem access ([[design/two-enforcers|two enforcers]]); an arbitrary
ancestor process that exported the public-name string switched it off
(deep-review findings S1, S2).

The fix authenticates the marker against a secret an external wrapper
cannot fabricate, leaving the rest of the enforcement model untouched.

- **A per-re-exec token.** `run_confined` (`sandbox/runner.rs`) mints 32
  bytes of OS entropy per confined re-exec (`sandbox/marker.rs`,
  `/dev/urandom` on Unix, `ProcessPrng` on Windows) and ships it inside
  the `ChildEvalRequest` — the IPC request frame, which crosses a
  socketpair / named pipe the parent ral owns. The token never rides the
  inheritable environment, so reaching it requires reading ral's private
  request channel, the same trust boundary the confined eval already
  rests on. The stronger alternative — an inherited file descriptor whose
  pipe-peer relationship a wrapper cannot forge — is recorded in the
  `marker` module doc; the token is the cheaper middle the review
  endorsed.
- **The child adopts, then verifies.** The confined child
  (`ipc::serve_from_env_fd` / `_handle`) calls `marker::adopt`: it records
  the token process-globally and stamps the same value into
  `RAL_SANDBOX_ACTIVE`. The transport gate (`runtime/transport::dispatch`)
  routes a restrictive grant body **local** only when
  `marker::authenticated()` holds — env value equals the recorded token,
  compared in constant time. A bare or forged marker has no recorded
  token, so it cannot authenticate and confinement is still attempted.
- **The marker scopes re-sandboxing only.** Even authenticated, the
  marker suppresses *re-entry* into the OS sandbox (redundant under bwrap,
  impossible under Seatbelt's one-shot `sandbox_init`); it never routes
  anything past the in-process `capability::check_*` gates, which keep
  folding the live grant stack. `Context::resolver_for_check` likewise
  switches to lexical resolution only under an *authenticated* marker, so
  a forged value cannot weaken the in-process fs gate's symlink handling.
- **The forged marker no longer survives the re-exec.** `run_confined`
  strips `RAL_SANDBOX_ACTIVE` from the child's environment before spawn,
  so an inherited forged value cannot trip the nested-respawn guard
  (`assert_not_already_confined`) and abort the very confinement it was
  meant to bypass. The child receives its authentic marker only by
  adopting the token.

**The fs floor for bundled coreutils follows from the above, not a
separate route.** A bundled `cat`/`diff`/`ls`/… has no in-process
`check_fs_*` and touches the filesystem through raw stdlib calls; its only
fs enforcer is the OS sandbox. Because every `fs`/`net`-restricting grant
body now dispatches into the confined child (the forged-marker shortcut is
closed), a bundled tool inside such a body runs — in-process via `uumain`
or via the pipeline-stage helper re-exec — under the inherited OS profile,
exactly as an external command does. The chokepoint stays compositional:
in-process checks for what ral interprets structurally, OS confinement for
everything that takes free-form argv.

Residual, platform-bounded: a *nested* grant that tightens fs **inside**
an existing Seatbelt child cannot re-tighten the OS profile (Seatbelt is
one-shot per process) — a limitation bundled tools share with external
commands, not new to this change.

See [[internals/capability-enforcement|capability-enforcement]],
[[design/two-enforcers|two enforcers]], [[map/core/capabilities|capabilities map]],
and the regression tests `core/tests/sandbox_forged_marker.rs` (S8) and
`core/tests/sandbox_nested_dispatch_local.rs`.
