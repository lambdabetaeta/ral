# No grant spells every program

**No grant value, in ral or from a host, can say "every program"; `Any` arises
only by extending an omitted exec axis.** `ExecAllow::Any`
(`authority/policy/exec.rs`) is made in one place: `ExecRules::join_top`, which
is `t ⊕ fragment` on exec, reached through `GrantLayer::extend`. This page is
the one home of that fact.

- **The decoder has no spelling for it.** An `exec` record has `allow` and
  `deny`; any other key, `every` among them, is refused
  (`decode_exec`, `unknown_exec_key`). A file key naming the root is refused
  with the way to say it: leave `exec` out for every program
  (`freeze::root_program`, `authority/grant/freeze.rs`, raised by
  `Keyed::file`, `authority/grant/resolve.rs`).
- **Where `Any` does arise.** `--extend` joins a `Fragment` onto a base whose
  exec axis is omitted (`Axis::join_top`, reached by `extend` in
  `authority/policy/lattice.rs`), and exarch's carve-outs decode `Axes` with only
  `deny` as a fragment (`decode_fragment`) over the top (`carve_out`,
  `GrantLayer::root().extend(..)` in `exarch/src/policy.rs`): every program, less the exclusions (a fragment whose
  exec lists nothing makes no `Any`). A policy's
  omitted axis is `t`, a fragment's is `⊥`, so the same record is a restriction
  or an extension by the role it is read in
  ([[design/authority-tables|authority-tables]]).
- **A `Fragment` is only extended.** It is a decoded layer read as an
  extension, whose omitted axis states nothing (`⊥`); only `GrantLayer::extend`
  consumes one, so no fragment is ever a layer of the stack.
- **`Any` has no negative form.** It is an `ExecAllow` and nothing is an
  `ExecDeny` of it, so "every program" can never be excluded, only lessened.
- **It shows as the extension that made it**: the layer's grant with exec left
  out, then the fragment of its other exec rules, written `grant G extended by F`
  ([[invariants/a-grant-has-one-outward-form|a-grant-has-one-outward-form]] owns
  that form).
- **Tests state it by building it:** `GrantLayer::with_every` is `cfg(test)`,
  because no grant decodes to it
  (`exec::tests::a_grant_resolves_both_sets_and_spells_no_any`). The inverse
  of a layer's value is tested on decoded layers and their extensions alone,
  never on `with_every`, whose table need not be any extension's
  (`value::tests::a_layers_value_decodes_back_to_it`).

**Why.** A flag a surface can set is a second spelling of an omitted axis, and
the two disagree as soon as a layer is stacked: an omitted exec axis is `t` and
restricts nothing, while a table holding `Any` is a supplied layer whose other
allowances still name run places. One origin keeps "restricts nothing" and
"runs everything" the same fact.

See [[invariants/a-grant-is-decoded-where-it-lands|a-grant-is-decoded-where-it-lands]],
[[design/grant|grant]],
[[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]].
