Pushed. Now to your question — **are there design pain points?** A few came up repeatedly while writing the fzf plugins:

1. **Piping user values into externals needs a magic classifier entry.** `to-lines $entries | fzf` only works because `to-lines` is hard-coded in `classify.rs` as `EncodeToBytes`. If a user writes their own function that produces bytes for a pipe, there's no way to declare "my block outputs `Mode::Bytes`". You'd have to alias a byte-producing primitive. An `@bytes` annotation on `let`, or an inferred output-mode from the body, would remove the gotcha.

2. **`return !{words $x}` is rejected** with "return expects a value; ! produces a computation", even though `!{…}` forces a computation *to* a value. That's exactly what `return` wants. I had to write `let w = words $x; return $w` every time. Either the parser should accept it, or the error should point to the intended idiom.

3. **Boolean polarity forces triple-nested forces.** `_if !{not !{is-empty $x}} {then} {else}` is "if x is NOT empty". Three forces and a `not` to express one predicate. A `when-nonempty $x {…} {…}` helper would help, but more usefully: `if` with an `else-if` form, or `when {…} {…}` / `unless {…} {…}` sugar, would cut the noise considerably.

4. **Silent `try { return $env[X] } { |_| return '' }` is almost always what I wanted.** Every fzf plugin opens with one. An `env-get X $default` builtin (or `$env?[X ?? '']`) would remove ten lines of boilerplate across three plugins.

5. **`ed-lbuffer`-style blocks rely on the "last command's return value is the block's value" rule** — but that rule isn't obvious. I kept wanting to write an explicit `return` and kept not doing it. If implicit-return-of-last is canonical, an `@returns-last` lint would stop me doubting; if explicit `return` is canonical, enforce it.

6. **TUI stdout capture is all-or-nothing.** `_editor 'tui'` captures every byte the body writes; if a plugin wanted to print a progress message to `/dev/tty` and *also* grab fzf's selection, it can't. Not a bug today, but will bite eventually. Allowing the body to split streams (one to terminal, one to the captured channel) would be cleaner.

Of these, **1** and **3** are the ones I think you'd feel most — **1** because it's a correctness trap (the plugin looks right and silently pipes empty), **3** because every non-trivial plugin has this shape. **4** is small but high-frequency.
