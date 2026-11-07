# An exclusion is located in one vocabulary

**Wherever a consumer needs the positions an exclusion occupies, it asks the
exclusion: `Excluded::holds` for every position it holds, `Walked::followed` for
the one an access lands on.** No backend keeps its own copy of where a deny lies.
Both live in `core/src/path/handle/exclusion.rs`.

- **`Excluded::holds`** is the name's hold and, where the name is a link, its
  target's, the name first. Matching reads it (`Excluded::matches`,
  `path/identity.rs`), so does the Unix projection (`Excluded::positions`, read by
  `SandboxProjection::rendered`), and so does Landlock's exec walk (`expand` and
  `listed`, `authority/sandbox/linux/landlock.rs`).
- **`Walked::followed`** is the link's target, else the object at the name: where
  an access to the spelling lands. The decoder's refusals judge it
  (`file_rules`, `Polar::grant`), and so does the Windows DACL side, which
  places each deny there, as `carve` does (`windows/session.rs::confine`,
  `sandbox/carve.rs::carve`).

**Why.** The enforcers must agree on where a deny is, or a deny held by one is
open in the other. The two readings differ exactly on a link: the Unix renderers
mask both its name and its target, while carving never grants a link entry and
`ensure_unreachable` refuses a link's own position, so the DACL side needs the
followed one alone. Each reading is therefore a named method on the type that
knows the answer, and a backend that wants a position picks a method, never
re-derives one by hand.

See [[invariants/grants-judge-objects|grants-judge-objects]],
[[internals/holding-a-deny|holding-a-deny]],
[[design/two-enforcers|two-enforcers]].
