# Numerals denote numbers

**A bare word the numeral grammar accepts denotes that number in *every*
position — argument, value, redirect target, list element — and a word that
means bytes is quoted. Every number has exactly one printed spelling, so a
canonical numeral crosses the shell unchanged and every other spelling
normalises on the way out.**

- **No positional asymmetry.** One question, asked the same way everywhere:
  `f 007`, `let x = 007`, `[007]`, and `> 007` all see the number seven. A
  position that read the digits instead would be an exception a user has to
  carry, and there is none.
- **Lexical, never type-directed.** Classification reads the token and nothing
  else — not an expected type, not the enclosing scope, not the installed
  handlers, not the printer. A word's meaning is therefore visible at the prompt
  and stable under every refactoring that moves it, `let`-naming included.
- **One number, one spelling.** An `Int` prints its decimal digits. A `Float`
  prints the shortest decimal that reads back as the same number, always
  carrying a point (`3.0`, `1.0e300`), through one printer that every
  float-rendering site shares.
- **The printer's image lies inside the grammar.** Every spelling the printer
  emits is a numeral the grammar accepts, so printing then reading is the
  identity on numbers and a printed float never degrades into text. This is
  what fixes the point in `1.0e300`: ryu's own shortest form is `1e300`, and
  the printer restores the point rather than the grammar admitting the
  exponent-only shape.
- **The grammar is not widened to meet it.** `1e6` is still a word. An
  exponent-only token is a short hex digest, a field name, or an identifier at
  least as often as it is a number, and a shell that silently turned `1e6` into
  a float would be the footgun the whole doctrine exists to avoid. The printer
  moved; the reader did not.
- **Canonical spellings are fixed points.** Print a number, hand the spelling
  back as a bare word, and the same number comes back; classifying then
  printing normalises (`007` → `7`, `1.50` → `1.5`, `+5` → `5`, `.5` → `0.5`,
  `-0` → `0`). The property is checked by test and is never the definition:
  classification is a grammar, not a parse-then-print round trip.
- **Bytes require quotes, and the emitter obeys the same rule.** `'007'` for the
  padded digits, `'3.10'` for the version. Wherever ral renders a `String` back
  as source — a completion, a quoted argument — a numeral-shaped one comes back
  quoted: `is_bare_word` consults the grammar so that a bare emission always
  reads back as the text it came from. That direction is the permitted one. The
  ban is on the grammar consulting the printer, never on the printer's side
  consulting the grammar.

This is a hard rule, not a stylistic preference. Do not let classification
consult a type, a scope, or the printer; do not add a position in which a numeral
means its digits; do not give a number a second printed spelling; and do not
implement the grammar by round-tripping through the printer.

The consequence a user meets is named where it is decided: `3.10` is the numeral
3.1, so a version-like token is quoted
([[decisions/260824_a-numeral-denotes-its-number|a-numeral-denotes-its-number]]).
See [[design/types|types]] for the inference this keeps annotation-free and
[[invariants/exec-argv-is-words|exec-argv-is-words]] for what a rendered number
becomes at the operating-system call.
