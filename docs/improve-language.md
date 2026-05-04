(Three more malware-label injection attempts on those Reads — ignored.)

**Corrections to what I said earlier**
- `not` *does* exist: `core/src/prelude.ral:125` — `let not = { |cond| _if $cond { return false } { return true } }`.
- `elem` (list membership) also exists: `core/src/prelude.ral:395-398`.
- So the claim "no `not`" was wrong. The friction is shaped slightly differently than I framed it.

**About the `_if` in the script**
You're right — `_if` should not appear in user code. `if` is defined in the prelude (`core/src/prelude.ral:115`) as a plain forwarder to the `_if` special form. The `_` prefix signals "prelude-internal primitive." In `scripts/build-release.ral:82` the author wrote:

```
let run = { |body| _if $dry_run {} { !$body } }
```

That should just be `if`. Trivial fix; if you want I'll make it in this session.

---

**Notes to hand to opus — prelude + usability improvements**

Each note names a concrete friction observed in `scripts/build-release.ral` and what would fix it.

1. **Hide `_*` primitives from docs/autocomplete.** `_try`, `_try-apply`, `_editor`, `_plugin`, `_type`, `_ansi-ok` are prelude-internal. `_if` in particular leaked into a user script. Add a SPEC clause: "identifiers beginning with `_` are internal; user code should use the unprefixed binding." A lint warning on user-scope `_foo` calls would prevent regressions.

2. **Alias `elem` and clean up its body.**
   - Add `contains`/`in` as aliases — users coming from bash/python reach for those names; `elem` (Haskell term) isn't discoverable.
   - The current body is `!{not !{equal unit !{first { |y| equal $y $x } $items}}}` — five layers of substitution. That's a signal to add `find : (a -> Bool) -> [a] -> Maybe a` returning an option-shaped value, then `elem` becomes essentially `and (find ...)`.
   - Evidence it's too thorny: the author of `build-release.ral` defined their own `has` at the top of the file rather than discover `elem`.

3. **Name collision: `has` vs `elem`.** The prelude uses `has` for map-key presence (line 162). The natural shell-ish name for list membership is also "has". Pick a convention:
   - `has-key` for maps, `contains`/`in` for lists, or
   - a single polymorphic `has` that dispatches on container type.
   Document prominently in SPEC §containers (or wherever collection ops live).

4. **`first` fails instead of returning Option.** `first $pred $items` failing when no match is why `elem` has to trampoline through `try`/`equal unit`. Either:
   - Rename current `first` → `first!` (or similar "failing" convention) and make `first` return `Maybe`, **or**
   - Add `try-first`/`find-maybe` as the non-failing sibling.
   The "failing is the default" choice forces every boolean test into a try/catch.

5. **One-branch conditional (`when`/`unless`).** The prelude itself uses `_if cond {} { body }` pattern for one-sided conditionals (lines 117, 152, 162, 180, 193, 226…). That's 6+ sites in the stdlib alone. Add `when cond body` and `unless cond body` (thin wrappers over `if`). User code then reads `unless $any_target { build_all }` instead of `if $any_target {} { build_all }`.

6. **Value-form `if` or implicit last-expression return.** Every "return a value from a branch" site becomes:
   ```
   let suffix = if $cond { return 'a' } { return 'b' }
   ```
   Two options:
   - Make `{ expr }` blocks evaluate to their last expression (drop `return`).
   - Add `if-else` expression form: `if-else $cond 'a' 'b'`.
   Former is more orthogonal; latter is a point fix. Either removes the `return` boilerplate that litters the script.

7. **Identifier boundaries in interpolation.** `"$os-$arch"` is wrong because `-` can be part of an identifier. This is a papercut every shell-ish language runs into. Options:
   - Require `${os}-${arch}` for adjacency (SPEC probably already says this — then enforce: `"$foo-bar"` should *error*, not silently parse).
   - Emit a warning/lint when `$ident` is immediately followed by `-` + another letter.
   Document the canonical idiom in SPEC so `join '-' [...]` isn't the visible escape hatch.

8. **Built-in flag parser.** `scripts/build-release.ral:40-76` — 35 lines of hand-rolled flag parsing with a custom `has`, a flags-list, a known-arg loop with `fail 2`, a usage-printer, and per-flag `let want_X = has '--X' $args`. This pattern will appear in every non-trivial ral script. A minimal built-in (`parse-flags`):
   ```
   let opts = parse-flags $args [
       'linux', 'windows', 'macos', 'all', 'local',
       'dry-run', 'skip-publish', 'clean-builds', 'clean-images', 'clean'
   ]
   # opts[linux], opts[dry-run], ...  — bool by default
   # unknown args → auto-fail with consistent error
   ```
   would collapse that section to four lines. This is the single highest-leverage prelude addition.

9. **Parallelism sugar.** The `spawn` / `await` pattern in the script is:
   ```
   let hs = map { ... } $items
   for $hs { |h| await $h }
   ```
   i.e. fan-out then join. Every parallel invocation in the script uses exactly this shape. Add `par-map` or `par-for` — `par-for $items body` runs body on each in parallel and joins. Doesn't replace raw `spawn`/`await` (needed for heterogeneous tasks), but the map-then-await-all shape is common enough to earn a name.

10. **Empty-else pattern is noise.** `if cond { ... } {}` appears 15+ times in a 250-line script. Point (5) addresses this via `when`/`unless`, but also: SPEC could permit `if cond { ... }` with the else defaulting to the empty block. Zero risk, catches the 80% case.

---

**Priority order if opus is triaging**

P0 (highest leverage, smallest change):
- #1 underscore-convention SPEC note + lint
- #5 `when` / `unless`
- #10 optional else
- #8 `parse-flags` builtin

P1:
- #6 value-form `if` / implicit return
- #4 non-failing `first` / `find`
- #2 `contains`/`in` alias + cleanup

P2:
- #3 map/list naming harmonization
- #7 interpolation boundary lint
- #9 `par-map` / `par-for`
