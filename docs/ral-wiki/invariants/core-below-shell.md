# Core's low modules stay below the shell

**Production code in `authority`, `egress`, `fact`, `host`, `io`, `own`, `path` and
`process` cannot reach `shell`, directly or transitively.**

- Lower modules exchange domain data: environment overlays, observations,
  cancellations and authority refusals. The shell interprets them as runtime
  values, errors and escapes.
- Intentional cycles among lower modules are permitted; this is a boundary,
  not a DAG requirement.
- Documentation links and test-only shell fixtures do not create production
  dependencies.

See [[map/core|core]], [[map/core/capabilities|capabilities]] and
[[internals/cancellation|cancellation]].
