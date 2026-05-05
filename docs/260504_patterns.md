Not wrong. The mismatch is with ral's current `case`, which is unusual.

## What `case` is today

`case scrut tab` where `tab` is a **tag-keyed record literal of handler thunks** (parser.rs:368-385). Dispatch is by variant tag; the destructure of the payload happens *inside the handler's lambda parameter*, using the regular irrefutable pattern grammar. So today:

```
case $opt [`Some: { |x| use $x }, `None: { echo nothing }]
```

This is "case = tag-indexed jump table; lambda = destructure". Two existing forms composed, not a new pattern dialect. The downside: `case` only sees variants. Plain lists, strings, plain maps have no `case`.

What you want is the ML-style form:

```
case $x {
  ['hello', [x, y], [port: a, config: b]] -> body
  _ -> default
}
```

This requires three things:

1. **Multi-arm `case` syntax** — pattern → body, tried in order. The grammar is the cheapest part.
2. **Refutable leaves** — a literal `'hello'` as a pattern means "test equality". Pattern IR gains a `Pattern::Literal` arm; `assign_pattern` becomes `try_match : Pattern × Value → Option<Bindings>`.
3. **Typechecker rework** — today the case checker pattern-matches on `Ty::Record(row of tag-keyed thunks)`. Replacing or extending this to cover arbitrary structural patterns means redoing exhaustiveness (CaseNotExhaustive, CaseLabelTypeMismatch in scheme.rs) for the general structural case.

## Easy?

- **Runtime: small.** `assign_pattern` is 90 lines; turning it into `try_match` plus a `Pattern::Literal` leaf is straightforward. The dispatcher becomes "first arm whose `try_match` returns `Some`."
- **Parser: small.** Add `Pattern::Literal` for string/number/bool leaves; reuse the existing list/map pattern productions; add multi-arm case syntax (likely `{ pat -> body, pat -> body }`).
- **Typechecker: medium-large.** Refutable-pattern exhaustiveness over arbitrary product/sum types is a real algorithm. Today's case has a closed-row-of-tags test; the general version needs sealed-row reasoning per arm. This is where the faff lives.
- **Elaborator: medium.** Today's case lowers to a tag-lookup. Multi-arm case lowers to a chain of `if try_match then bind+body else next_arm`. Doable; not trivial.

## Philosophy?

**Aligned more than you'd think.** Two arguments:

- ML-style structural case *is* the Plotkin elimination form for sums + products. The tag-table form is the unusual one — it's a clever encoding, but it's not the textbook rule.
- ral's "irrefutable patterns" claim is already elastic. `let [a, b] = [1]` is a runtime failure today. So adding `Pattern::Literal` as a refutable leaf doesn't open a door that's currently closed; it just lets the user *write* the test that the runtime already knows how to fail at.

**Tensions, not blockers:**

- The tag-table form is a real expressiveness win the ML form lacks: the table is a first-class value, buildable dynamically (`let d = [\`Some: …]; case $x $d`). If you go ML-style, you lose that — unless you keep both forms, which is the "three pattern languages" mess to avoid.
- Two case forms makes the language harder to teach. One unified form is cleaner but the rewrite is the price.

## My read

This is a good idea but it's a project, not an afternoon. The path is:

1. Add `Pattern::Literal` (string / number / bool / unit) — an entry in the existing irrefutable grammar; refutable at the leaf, like list-length already is.
2. Add multi-arm case parsing.
3. Make `try_match` the unified runtime primitive; today's `assign_pattern` becomes `try_match` followed by an unwrap that errors on `None` (preserving `let` semantics).
4. Subsume the tag-table form: `case x { \`Some y -> body }` is just a map-pattern with a tag key. Drop the special-case typecheck pass.
5. Rebuild exhaustiveness on the unified form.

Step 5 is where the faff is. Steps 1-4 are mostly pleasing.

Net: fits the philosophy, lands as one cleaner system rather than three. Worth doing if you can spend a week on it. Don't do it half: leaving the variant-table form in alongside multi-arm case is the worst of both worlds.
