---
status: active
---

# A numeral denotes its number, in every position

**A bare word the numeral grammar accepts *is* that number — as an argument, as
a value, as a redirect target, as a list element — and there is no position
where it means its digits instead. A word that means bytes is quoted: `'007'`.
The complement is that every number has exactly one printed spelling, so a
canonical numeral crosses the shell byte-identically and every other spelling
normalises on the way out.** Classification is a lexical grammar over the token
alone, so what a word denotes is decided by what is at the prompt — and the type
system stays plain Hindley–Milner, with no defaulting on value types, no
classes, and principal types intact.

## The question

ral is implicitly typed, has no annotations, and its leaves are bare words. `007`
is therefore the one place where a shell's surface and an inferred language's
discipline have to be reconciled: something must say what the leaf means, and
there are exactly two candidates — the *grammar* or the *checker*. The recent
answers all put the meaning in syntax: what a `case` arm is
([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]), what
an argv's elements are
([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]), what
a coercion is
([[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]]). This is the
same answer for the leaf, and it was left open at
[[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]], which named the
two type-directed candidates and took neither.

The grammar answers.

## The doctrine

- **One rule, every position.** A word is classified before anything knows a
  head, a type, or a scope. Value position and argument position ask the same
  question and get the same answer, so there is no positional asymmetry to
  remember, nothing to look up, and nowhere for an exception to hide.
- **The grammar is small and stated once**, in `docs/SPEC.md` §4.1: an optional
  sign and digits is an `Int`; a decimal point with digits on at least one side,
  optionally followed by an exponent, is a `Float`. Everything else is a word —
  an exponent with no point (`1e6`), a separator (`1_000`), another base
  (`0x10`), and any spelling outside its kind's range.
- **Quoting is the only escape, and it is total.** `'007'` is those three bytes;
  `#'3.10'#` is those four characters. A user who means text says so once, in
  the token, where the next reader can see it.
- **One number, one printed spelling.** An `Int` prints its decimal digits. A
  `Float` prints the shortest decimal that reads back as the same number, always
  keeping its point — `3.0` prints `3.0`, `1.0e300` prints `1.0e300` — so no
  float ever prints a spelling that would read back as an `Int`. Every
  float-rendering site goes through that one printer, so `echo`, `to-json`, and
  an external's argv cannot disagree about a number.
- **The printer's image lies inside the grammar.** Every spelling the printer
  emits is a numeral the grammar accepts, so printing then reading is the
  identity on numbers. The grammar was *not* widened to achieve this: `1e6`
  remains a word, and it is the printer that restores the point ryu's shortest
  form omits.
- **A non-canonical numeral normalises on output**, because it denotes a number
  and a number has one spelling:

| written | printed |
| --- | --- |
| `007` | `7` |
| `1.50` | `1.5` |
| `+5` | `5` |
| `.5` | `0.5` |
| `-0` | `0` |

- **Canonical spellings are fixed points — and that is a test, not a
  definition.** Print a number, hand the spelling back as a bare word, and the
  same bytes come out. Classification is never *implemented* as parse-then-print:
  a grammar that asks the printer what a token means has no context-free
  statement of itself, cannot explain its own refusals, and makes the round trip
  the mechanism instead of the property.

## Why this is not fifty years of quoting

Bash's quoting is a defence against invisible dynamic content: `$x` splits and
globs according to what it happens to hold, so the same line is harmless or
catastrophic depending on data the reader cannot see, and the discipline becomes
a habit kept everywhere because there is no telling where it matters. ral's
quoting defends against a **static, context-free property of a token in front of
you**. `007` is a number for the same reason in every program that contains it;
deciding whether to quote requires reading nothing but the word. One rule, all
positions, no exceptions — the only kind of rule a shell user can actually hold.

## The sharp edge, named

**`3.10` is the numeral 3.1.** A version, a semver bound, a padded field width:
whatever the author meant, the grammar sees a float and the shell prints `3.1`.
Such tokens are quoted — `'3.10'` — and the documentation *teaches that example*
rather than merely stating the rule, because it is the one case where intent and
grammar disagree and the grammar wins quietly. exarch's line witness is built
from the other side of the same fact: `h` plus six hex can never be a numeral, so
it needs no quoting discipline at any call site
([[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]]).

The second consequence is at the exec boundary: a whole `Float` reaches an
external's argv with its point on (`3.0`, not `3`), since that argv is rendered
by the same printer as everything else
([[invariants/exec-argv-is-words|exec-argv-is-words]]). A caller who needs the
integer spelling passes an integer.

The third edge is where the two halves of the doctrine had to be reconciled, and
the resolution is worth recording because the obvious repair was the wrong one. A
float numeral must carry a decimal point, while ryu's shortest form for an
extreme magnitude carries none — `1e300`, `1e-7` — so for a while a printed float
of that shape read back as *text*. Two cures were available: widen the grammar to
admit exponent-only numerals, or make the printer emit the point. Widening was
refused. An exponent-only token is a short hex digest, an identifier, or a field
name at least as often as it is a number, and a shell that quietly turned `1e6`
into a float would be committing the very data-corruption footgun the doctrine
exists to prevent — the one every spreadsheet and YAML loader is famous for. So
the printer moved instead: it restores the point, `1e300` becoming `1.0e300`, and
the printer's whole image now lies inside the grammar. Printing then reading is
the identity on numbers; `1e6` is still a word.

The dependency runs one way only, and that is what makes the repair legitimate.
The printer may be built to land inside the grammar, and the emitter that renders
a `String` back as source may consult the grammar to decide whether the bytes can
go bare — `is_bare_word` does exactly that, so `007` and `3.10` come back quoted
rather than as numbers. What stays forbidden is the reverse: a grammar that asks
the printer what a token means.

## Alternatives considered

### 1. Uncommitted words, decided by the checker

Elaborate a bare word to an uncommitted `Word`, let inference decide it from use,
and default what is still undecided at each generalisation boundary:
unconstrained ⇒ bytes, numeric ⇒ `Int`. It is the shape an inferred language
suggests first, and it imports Haskell's defaulting corner in miniature.

- **Principality is lost across the default.** The scheme a boundary publishes
  would depend on a rule of the language rather than on the program, so a leaf's
  meaning becomes a property of *where it was generalised*.
- **`let`-naming stops preserving meaning.** An intermediate name commits a word
  that the anonymous form leaves open, so `f 007` and `let x = 007 ; f $x` need
  not agree — and naming an intermediate is the refactoring a shell user performs
  most often.
- **A monomorphism-restriction-shaped fork.** Whether the numeric constraint
  outlives quantification has to be decided one way or the other, and both
  answers are the wrong kind of famous.
- **The route solver acquires a partner.** Boundary drains would have to reach a
  joint fixpoint with the payload-route solver
  ([[internals/type-inference|type-inference]]): two defaulting disciplines on
  two lattices, settling together. ral's one declared default is the route's —
  positional, height-1, stated. That is the most a default may cost.
- **Diagnostics get worse exactly where they matter.** A post-commitment type
  error can no longer name the word that caused it: the token is an `Int` by the
  time the clash surfaces, and the message is about a type the user never wrote.

A plan titled "a word is uncommitted" explored this at length and is deleted;
this page is its surviving record.

### 2. A word is a numeral iff it is canonical

Read a numeral as *the canonical rendering of a number*, and `007` is a string
while `7` is a number: the sharp edge and the whole normalisation table vanish,
because the only numerals are the already-canonical spellings. Rejected on both
halves. `007` is ruled to be a number — a padded numeral is a numeral, and
someone who writes `007` in arithmetic has not made a category error. And
classification must not couple to the printer: the round trip is a property the
two are obliged to have, never the mechanism by which either works.

### 3. Qualified literals with evidence

A real `fromInteger`: a literal is one occurrence carrying a class constraint,
taking a different value at each instantiation. This is the *principled* version
of alternative 1 rather than a variant of it — with evidence, `007` in a
`String`-typed hole is an instance, not a default, and none of that alternative's
five costs is paid. It is also a much larger language than a shell needs today:
classes, evidence at runtime, and a defaulting story anyway for the
unconstrained cases. **Explicitly not foreclosed.** Should ral grow qualified
types for some other reason, this is the answer to revisit, and the doctrine
costs nothing in the meantime: a numeral that denotes its number is exactly the
integer instance.

### 4. Head-directed elaboration

nushell's answer: let the elaborator consult the head and classify each argument
by what that head expects. Rejected because the elaborator can resolve builtins
and externals but *not* bound names — a `let`-bound function, a handler installed
at runtime, a PATH lookup — so the head is known for some calls and unknown for
others, and two mechanisms end up deciding one question. Which of them answered
would then be visible in the meaning of the program.

## Recorded and not taken: the trailing-zero restriction

A numeral could be defined to forbid a redundant trailing zero after the point,
making `3.10` and `1.50` strings by syntax alone. It kills the sharp edge without
asking the printer anything and stays purely lexical. Not taken: it buys two
spellings at the price of a numeral grammar with a side condition — one that no
longer means "looks like a number" — and `007`, a *leading* zero, is a number
regardless, so the discipline the user must learn is unchanged while the rule
they must learn is longer. Kept on the record as a real option should the
version-string case prove to bite in practice.

## Corollary: `unit` is not a literal

Landing beside this: **`unit` stops being a word literal.** A literal whose
printed form is nothing cannot obey the one-spelling law from either end — there
is no spelling to read back — so `()` joins `[]` and `[:]` as punctuation, and
`unit` becomes an ordinary word (`echo unit` prints `unit`). The word literals
are then exactly the numerals, `true`, and `false`.

`()` prints as itself wherever a value is rendered as text — the interactive
renderer, `Display` (hence argv: `echo a () b` prints `a () b`), and string
interpolation — with no special case anywhere; the interpolation arm that
rendered a unit as the empty string is gone. The one place a unit is *not*
shown is the REPL's result echo, which suppresses unit results so that `cd`
prints nothing at the prompt; that is a rule about which results the prompt
echoes, not about how a unit renders, and it stands.

## See also

[[invariants/numerals-denote-numbers|numerals-denote-numbers]] (the rule as an
invariant),
[[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]] (its open
language question, answered: declined in favour of literal syntax),
[[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]] (the
rendering that carries a number into an external's argv),
[[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]] (the same doctrine
one level up: a meaning the ambient session can change is not a meaning),
[[design/types|types]], [[internals/surface-syntax|surface-syntax]],
[[internals/type-inference|type-inference]]; `docs/SPEC.md` §4.1, §3.
