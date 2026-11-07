# No grant spells every program

**No grant value, in ral or from a host, can say "every program"; `Any` arises
only by extending an omitted exec axis.** `ExecAllow::Any`
(`authority/policy/exec.rs`) is made in one place: `ExecRules::join_top`, which
is `t ⊕ fragment` on exec, reached through `GrantLayer::extend`.

- **The decoder has no spelling for it.** An `exec` record has `allow` and
  `deny`; any other key, `every` among them, is refused
  (`decode_exec`, `unknown_exec_key`). A file key naming the root is refused
  with the way to say it: leave `exec` out for every program
  (`PolicyError::root_program`, `authority/grant/freeze.rs`).
- **Where `Any` does arise.** `--extend` joins a `Fragment` onto a base whose
  exec axis is omitted, and exarch's carve-outs decode `Axes` with only
  `deny` as a fragment (`decode_fragment`) over the top (`carve_out`,
  `GrantLayer::root().extend(..)` in `exarch/src/policy.rs`): every program, less the exclusions. A policy's
  omitted axis is `t`, a fragment's is `⊥`, so the same record is a restriction
  or an extension by the role it is read in
  ([[design/authority-tables|authority-tables]]).
- **`Any` has no negative form.** It is an `ExecAllow` and nothing is an
  `ExecDeny` of it, so "every program" can never be excluded, only lessened.
- **Tests state it by building it:** `GrantLayer::with_every` is `cfg(test)`,
  because no grant decodes to it
  (`exec::tests::a_grant_resolves_both_sets_and_spells_no_any`).

**Why.** A flag a surface can set is a second spelling of an omitted axis, and
the two disagree as soon as a layer is stacked: an omitted exec axis is `t` and
restricts nothing, while a table holding `Any` is a supplied layer whose other
allowances still name run places. One origin keeps "restricts nothing" and
"runs everything" the same fact.

See [[invariants/a-grant-is-decoded-where-it-lands|a-grant-is-decoded-where-it-lands]],
[[design/grant|grant]],
[[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]].
