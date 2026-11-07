# Core's low modules stay below the shell

**Production code in `authority`, `egress`, `fact`, `host`, `io`, `own`, `path` and
`process` cannot reach `shell`, directly or transitively.**

- Lower modules exchange domain data: environment overlays, observations,
  cancellations and authority refusals. The shell interprets them as runtime
  values, errors and escapes.
- Intentional cycles among lower modules are permitted; this is a boundary,
  not a DAG requirement.
- `data`, which they all use, stays below the shell too: the one value
  printer (`data::print`) reads a value through a `Printable` view the shell's
  `Value` implements, so `authority` prints a grant without the shell.
- `core/tests/layering.rs` enforces it for these modules and `data`: no
  production `crate::shell` path, or `super::` path reaching it, in any of them.
- Documentation links and test-only shell fixtures do not create production
  dependencies.

See [[map/core|core]], [[map/core/capabilities|capabilities]] and
[[internals/cancellation|cancellation]].
