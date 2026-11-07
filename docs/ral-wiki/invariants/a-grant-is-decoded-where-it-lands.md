# A grant is decoded where it lands

**A grant crosses every boundary as a *grant value*, plain data, and is resolved
by the one decoder on the host that enforces it.** No host resolves a grant for
another, and no second route builds a layer: whoever authors the grant, a
`GrantLayer` is the output of the decoder walker `decode_capability_map`
(`core/src/shell/grant/decode.rs`, crate-private) and of nothing else.

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
- **Every boundary is covered.** synod's ceiling (`ceiling`,
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

**Why.** A position resolved on one host means nothing on another, and a second
route is a second set of refusals. The serde route was laxer than the decoder:
it admitted a file key naming a directory, a path-shaped name, and a spelling
absolute only on another platform, so what a ral author could not write, a
host could send. With one front-end the refusals hold everywhere (below), and
the wire carries spellings, never positions or identities.

**What the decoder refuses, on every route.**

- **A file key naming a directory** (`file_rules`): it names a binary that
  cannot exist; the hint says `` `dir ``. The root is one such directory
  (`PolicyError::root_program`).
- **A path-shaped name** (`bare_name`, `path_shaped`): `` `name `` takes a bare
  command name. Shape is judged under Windows rules on every host, so a profile
  that loads on Linux refuses alike on Windows.
- **An exclusion of the root** (`Polar::root` and `Polar::grant` for `Excluded`, `authority/grant/paths.rs`): the root
  holds no object to name, and a link to the root is the root under another
  name. The root is the raw `/`, an `absolute` payload tested as a string before
  any classification or fold (`FreezeCtx::walked`, `Placed::Root`), so it means
  everywhere on every host and no foreign spelling folds to it.
- **A spelling absolute only on another platform** is the *dead grant*:
  `FreezeCtx::walked` answers `None` (`live_absolute`), the key makes no rule and
  the profile still loads on both. A path the host itself holds is never dead:
  `host_absolute` refuses it, because a deny that named nothing would fail open.

See [[design/capability-freeze|capability-freeze]] (the decode),
[[design/grant|grant]], [[invariants/no-grant-spells-every-program|no-grant-spells-every-program]],
[[design/engine-protocol|engine-protocol]] (what crosses).
