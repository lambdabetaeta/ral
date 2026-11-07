# A grant has one outward form

**Whatever shows a grant shows its grant value, printed by ral's one value
printer; no second notation spells one.** This page is the one home of a
grant's printed form. The dual of
[[invariants/a-grant-is-decoded-where-it-lands|a-grant-is-decoded-where-it-lands]]:
a grant comes in as a value and goes out as one.

- **A layer shows itself as decode's inverse.** `GrantLayer::value`
  (`authority/policy/lattice.rs`) builds a `LayerValue` through the typed
  encoder (`authority/grant/value.rs`) from each rule's `written` spelling or
  key, and `decode_grant(value(l)) == l` on the host that decoded `l`
  (`value::tests::a_layers_value_decodes_back_to_it`, over decoded layers and
  their extensions by decoded fragments).
- **A rule prints resolved.** An anchored place shows as the `absolute`
  spelling it froze to (`` `cwd src `` as `` `absolute '/work/src' ``), a
  selector as the directories it chose, a bare name, allowed or excluded, as
  written: an excluded name also shows the file it was found as, and its veto
  (`exec::Veto`) judges by a folded key while showing the name as written. A
  rule keeps no `Place`: after a `cd`, `` `cwd src `` would predict the wrong
  denial.
- **An empty list is left out.** The decoder reads an omitted table list as
  the empty one, so the encoder omits it (`read: []` beside a `write` list
  would read to a model as "reads nothing"); a table listing nothing is
  written by its first list, `[allow: []]` or `[read: []]`, as `grant` writes
  the empty table. The property test decodes every value with each omitted
  list written out empty, alike.
- **Every program shows as an extension.** No grant spells `Any`
  ([[invariants/no-grant-spells-every-program|no-grant-spells-every-program]]),
  and `Any` arises only as `t ⊕ fragment` on exec, the table's other rules
  being the fragment's. So a layer whose exec holds `Any` shows as its grant,
  exec left out, then the fragment `[exec: <those rules>]`:
  `decode_grant(grant) ◁ decode_fragment(fragment)`. Nothing beside the
  resolution is kept; every other extension is spelled (`b ⊕ f` as the union,
  fs over the top as `` write: [`absolute '/'] `` beside the exclusions).
- **Every reader takes it.** `Probe::Grants` answers each layer a reader
  needs (`GrantLayer::is_shown`: it restricts, or its exec runs something,
  which stays readable under a nested fs restriction) as its `LayerValue`,
  with the host's `withholds_gpu` (`Grants`, `protocol/probe.rs`). The engine's
  prompt, exarch's and a synod guest's alike, prints each in `grant` syntax,
  oldest first, an extension as `grant G extended by F`
  (`Display for LayerValue`, `authority/grant/value.rs`), under a legend that
  says only what grants mean (`exarch/data/grant-legend.md`; `grant_section`,
  `exarch/src/prompt.rs`). A refusal citing an exec exclusion prints it as
  its value (`Display for ExecDeny`), and so does the file-key refusal.
- **The printer is beneath `authority`.** `data::print::pretty_print` reads a
  value through a `Printable` view that `FOValue` and the shell's `Value` each
  implement, so `authority` prints a grant without reaching the shell
  ([[invariants/core-below-shell|core-below-shell]]), and the REPL and a grant
  share one printer.
- **Paths are shown with a slash.** An `absolute` payload is the spelling as
  ral shows it ([[invariants/paths-are-shown-with-slash|paths-are-shown-with-slash]]),
  which decodes back to the same spelling.

**Why.** Three forms (the value, a summary of key strings, and a prompt
notation of `exec dirs:`, `(none)`, `inherit`) were three vocabularies of one
authority, each with its own rendering of an anchored place and of every
program, and a prompt the model could not paste back into `grant`. One form
means the agent reads its authority in the syntax it writes a narrower grant
in, and a refusal names a rule the way the prompt does.

**Gap.** The inverse is exact on the decoding host, its filesystem and `PATH`
unchanged, but for one spelling that means the same: on Windows a verbatim
`\\?\C:\…` spelling shows without its prefix.

See [[design/grant|grant]], [[design/engine-protocol|engine-protocol]],
[[map/exarch/policy|exarch policy]].
