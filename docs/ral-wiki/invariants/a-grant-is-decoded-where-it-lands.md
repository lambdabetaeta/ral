# A grant is decoded where it lands

**Newly authored policy crosses boundaries as a *grant value*, plain data,
and is resolved by the one decoder on the host that enforces it.** No host
resolves authored policy for another host. A new `GrantLayer` comes from the
decoder walker `decode_capability_map` (`core/src/shell/grant/decode.rs`,
crate-private), the ambient root layer that says nothing (`GrantLayer::root`,
a spawn's `Inherit`), or a decoded layer extended by a decoded fragment
(`GrantLayer::extend`). Transfer of already-resolved state on the same host
is separate from authoring policy.

- **One front-end, many authors.** `grant [...] { body }`
  (`shell/evaluator/machine.rs`), profiles (`shell/load/profile.rs`), a spawn's
  restriction (`SpawnGrant::layer`, `shell/grant/spawn.rs`) and every grant value
  a host writes for an engine go through it. A host-authored value enters by
  `decode_grant` or, read as an extension, `decode_fragment`
  (`shell/grant/decode.rs`), the only public entries; `Shell::decode_grant`
  (`shell/grant.rs`) is `decode_grant` against the receiving shell's own anchor
  (`Shell::freeze_ctx`: environment overlay, logical cwd, scratch).
- **A host writes a grant value only by encoding typed axes.** `GrantValue` is
  made only from `Axes` (`ExecTable`, `FsTable`, `Target`, `Place`) in
  `authority/grant/value.rs`, so no host spells the record by hand.
- **Authored policy uses this boundary.** synod's ceiling (`ceiling`,
  `synod/src/folder/grant.rs`) rides `Root.config` and is decoded by
  `engine_root` (`exarch/src/boot.rs`); exarch's boot grant is its profiles
  through the loader and a root's ceiling through `engine_root`; exarch's
  carve-outs, the credential and restrict-file denies and the self layer, are
  `Axes` that `carve_out` (`exarch/src/policy.rs`) encodes and decodes with
  `decode_fragment`; `GrantValue::deny_all` is decoded by
  `evaluate_no_authority` (`exarch/src/config.rs`).
- **`Capabilities` is a test builder.** `Capabilities`, `ExecGrant`, `ExecKey`
  and `FsPolicy` exist only under `cfg(any(test, feature = "test-util"))`
  (`authority/grant/builder.rs`). `Capabilities::value` spells a `GrantValue`
  and `test_helper::decoded` sends it through `decode_grant`, so a test
  grant meets the refusals a ral author meets.
- **The anchor is the engine's own.** A ceiling is resolved against the shell
  that enforces it, so `cwd` and `scratch` places work in it and mean the
  engine's own cwd and scratch: in a synod guest the guest's, never the host's
  (`exarch/src/boot.rs::a_ceiling_resolves_against_the_engines_own_anchor`).
- **Resolved state stays on its host.** `Root.grants` transports a resolved
  `GrantStack` (`core/src/protocol.rs`), not an authored grant value. The engine
  clones it, reholds its exclusions and installs its layers after the root
  recipe, without grant-value decoding (`core/src/engine.rs`). These layers
  are resolved on the engine's host and mean nothing on another; like a sealed
  child's grants, they are execution state
  ([[design/engine-protocol|engine-protocol]]), not a second policy front-end.
- **Authored policy leaves as it came.** The wire form of a ceiling or a boot
  grant stays the author's value and is never derived from a layer. What a layer
  shows of itself is the value that decodes back to it, on the host that decoded it
  ([[invariants/a-grant-has-one-outward-form|a-grant-has-one-outward-form]]).

**Why.** A position resolved on one host means nothing on another, and a second
policy front-end is a second set of refusals. With one front-end the refusals
hold for every author (below), and authored policy crosses hosts as spellings,
not resolved positions or identities. Same-host resolved-state transfer does
not reinterpret those spellings.

**What the decoder refuses, on every authoring route.**

- **A file key naming a directory** (`file_rules`): it names a binary that
  cannot exist; the hint says `` `dir ``. The root is one such directory, whose
  refusal is `freeze::root_program` (`authority/grant/freeze.rs`), raised where
  a walked file key becomes a rule (`Keyed::file`, `authority/grant/resolve.rs`).
- **A path-shaped name** (`bare_name`, `path_shaped`): `` `name `` takes a bare
  command name. Shape is judged under Windows rules on every host, so a profile
  that loads on Linux refuses alike on Windows.
- **An exclusion of the root** (`Polar::root` and `Polar::grant` for `Excluded`, `authority/grant/paths.rs`): the root
  holds no object to name, and a link to the root is the root under another
  name. The root is the raw `/`, an `absolute` payload tested as a string before
  any classification or fold (`Placed::absolute`, `Placed::Root`), so it means
  everywhere on every host and no foreign spelling folds to it.
- **A spelling absolute only on another platform** is the *dead grant*:
  `FreezeCtx::walked` answers `None` (`live_absolute`), the key makes no rule and
  the profile still loads on both. A path the host itself holds or chooses is
  never dead: `host_absolute`, the one classifier of such paths, refuses it,
  because a deny that named nothing would fail open.

See [[design/capability-freeze|capability-freeze]] (the decode),
[[design/grant|grant]], [[invariants/no-grant-spells-every-program|no-grant-spells-every-program]],
[[invariants/a-grant-has-one-outward-form|a-grant-has-one-outward-form]] (the
way out), [[design/engine-protocol|engine-protocol]] (what crosses).
