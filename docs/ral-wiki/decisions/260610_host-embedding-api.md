---
status: active
---

# Hosting a Shell is a baked prelude plus one boot call

**Embedding `ral-core` is `static PRELUDE = baked_prelude!();
boot_shell(terminal, &PRELUDE)` and the existing per-turn eval entries —
the rest is host-specific.** The REPL host (`ral`), the agent host
(`exarch`, [[design/exarch-architecture|exarch-architecture]]), and the
test harnesses all spelled the same embedding ritual structurally: two
`postcard` blobs behind two `OnceLock`s, then `Shell::new → seed env →
register prelude → register type hints`. `core/src/host.rs` names the
missing type and the one function that deduplicate it.

- **`BakedPrelude` is the missing type.** It holds the build-time IR and
  scheme blobs and their once-decoded forms (`from_blobs` const,
  lazy `comp()` / `schemes()`); `bake_runtime()` bakes from the embedded
  prelude source for test binaries, which have no build-time blob. The
  `baked_prelude!()` macro expands `include_bytes!(OUT_DIR/…)` in the host
  crate (each crate has its own `OUT_DIR`).
- **`boot_shell(terminal, &BakedPrelude) -> Shell` is the kernel.**
  Construct, seed default env, register the prelude, register its type
  hints — everything a turn-evaluating host needs before its own
  builtins, rc files, or capability frames. Per-turn evaluation reuses the
  existing `compile_and_typecheck` / `session_schemes` / `eval_top_level`
  entries unchanged.
- **The bake moves into core, both halves.** `bake_prelude_to_out_dir()`
  is the build-script half (parse → elaborate → bake → two blobs into
  `OUT_DIR`, with rerun-if-changed on the shape-defining files); each
  host's `build.rs` shrinks to one call. `postcard` leaves both hosts and
  enters core once.
- **The schema-evolution hazard collapses to one file.** The baked
  prelude is a schema-less blob: a shape change to the IR or scheme types
  silently corrupts a decode unless the rerun-if-changed list pins every
  shape file. That contract had been spelled in three places that must
  agree; with the only encode site and the only decode site in
  `host.rs`, it is one list ([[internals/compilation-ladder|compilation-ladder]]).
- **No builder, no config bag.** The API is exactly the missing type and
  the one kernel function; the divergent steps — terminal handling, rc
  sourcing, the agent watchdog and library, output capture — stay in the
  hosts, interposed before and after the call.

One behavioural shift, verified inert: because `boot_shell` registers the
prelude (which pushes the user scope) before the host binds `TERMINAL`,
that binding now lands in the user scope rather than the prelude scope.
Lookup, completion, and scheme seeding all walk every scope, so the result
is identical; only `which`'s provenance label for `TERMINAL` shifts from
`prelude` to `local`, which is the more accurate description of a
host-injected runtime value ([[map/core|core]], [[map/repl|repl]]).
