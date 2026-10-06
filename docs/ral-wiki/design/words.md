# Words

**A word is one atom: a bare word, a literal `'…'`, an interpolation `"…"`,
or a splice (`$name`, `$[…]`, `!{…}`). Whitespace separates words. Two
atoms with nothing between them are a parse error, never two arguments and
never one.** Text is built in exactly one place, inside `"…"`, and that
boundary is visible in the source. There is no gluing.

## The rule

§3.5 of the spec lists the characters that end a bare word: `'`, `"`, `$`,
`!`, and the structural punctuation. That list is right and stays. What it
describes, though, is where an *atom* ends, and an atom is not yet a word: the
rule above adds the word level. Where the grammar itself attaches one form to
another — an index after a variable or a forced block (`$h[k]`, `!{f}[k]`), a
spread before a list (`...$xs`), a redirect before its target (`>file`) —
adjacency is a production and is unaffected. Everywhere else, an atom
touching an atom is refused:

```text
--prefix=$d            error: `--prefix=` and `$d` touch
--prefix='/opt/my dir' error: `--prefix=` and `'/opt/my dir'` touch
$dir/file              error: `$dir` and `/file` touch
$n.txt                 error: `$n` and `.txt` touch
'a'"b"'c'              error: `'a'` and `"b"` touch
```

The diagnostic names both readings, because the parser cannot know which was
meant: *insert a space if these are two arguments; write `"--prefix=$d"` if
they are one.* The suggested spelling is built from the parsed atoms, not by
wrapping the source text in quotes: `C:\tmp\$leaf` must come back as
`"C:\\tmp\\$leaf"`, since a bare word processes no escapes and an
interpolation does.

Two things that look like the same question are not. `a\ b` is two words,
`a\` and `b`: backslash is an ordinary character so that `C:\Users\x` is one
word, and the space between them is a real separator. And sh's
`'it'\''s'` — what `git`, Python's `shlex.quote`, and Ansible emit for an
argument containing an apostrophe — is an unterminated string here, as it
always was; see *The `-c` door* below.

## Why

**Passing a value and constructing text are different operations, and the
source should show which one is happening.** `$x` passes a value of any type;
`"$x"` renders it as text, and a list is a type error there. A rule under
which `$x` keeps its type but `$x/` or `$x''` silently becomes text makes
contact between fragments an implicit conversion operator, with its own
three-way typing (a bare numeral is a number, a lone splice is its value,
anything compound is text). ral has one conversion, interpolation, and one
syntax for it.

**Removing word splitting removes sh's worst hazard, not every hazard.** What
makes `--prefix=$d` dangerous in sh is that `$d` splits after substitution,
so gluing a split thing is a lottery and everyone quotes defensively. ral
never splits: with `d = 'x y'`, `echo $d` passes one argument. That makes
gluing *harmless*, and it is tempting to conclude that gluing is therefore
fine. But a missing space between two quoted arguments would then be valid
concatenation, and no external command's typing can tell the resulting wrong
string from an intended one. `printf '%s\n'"$x"` would be one format argument
and no data. The rule above catches that before anything runs.

**Partial quoting must never become house style.** Once fragments glue,
"quote the parts that need it" is legal, `'--prefix='$d` and `'a'"b"` are
idiomatic, and there is no principled line between the readable
`--prefix='/opt/my dir'` and the unreadable `'a'"b"'c'`. That is where the
quoting folklore of the last fifty years comes from, not from gluing itself.
Keeping a quoted string a whole word keeps the discipline: *separate
arguments; put constructed text inside an interpolation.*

**The failures that motivated this were silent.** An agent with sh habits
writes `$dir/file` constantly and got `[/tmp] [/x]` without a word of warning;
the system prompt carried a line telling it to quote composite paths. A
recoverable error before execution, with the correct spelling in it, is the
fix for silence. Whether the retry it costs is a measurable tax on an agent
is a prediction; silent wrong arguments are a fact.

## Alternatives

**Touching fragments form one word, meaning their interpolation.** The sh
rule minus splitting: `--prefix=$d` is `"--prefix=$d"`, `'a'"b"'c'` is
`"abc"`, `1'2'` is the text `12`. It reuses interpolation's semantics and the
IR already has the node. It was declined for the three reasons above, and
because the slogan under which it is attractive, *a word is an interpolation
and quotes only admit whitespace*, leaks everywhere it is pressed: a bare
fragment processes no escapes while `"…"` does; quotes admit `:` before
space, `,` inside `[…]`, `...` at word start, and `#`, not only whitespace;
`'…'` suppresses interpolation rather than admitting anything; tilde needs
the notion *bare fragment at word start*, which a string has not; and a lone
bare fragment is a literal (`1` is an Int, `3.10` is 3.1) while `"1"` is
text, so the slogan is false for exactly the words that are not
interpolations. An honest statement is *a standalone atom keeps its meaning;
adjacent fragments build one string by interpolation's scalar conversions;
quoting and postfix syntax keep their own rules* — implementable, and not
one rule.

**A bare word is a double-quoted string without the quotes; a quote is a
boundary.** Splices are terms, quotes are delimiters of literal text, so
`$dir/file`, `$n.txt`, `--prefix=$d`, `x=$[1+1]`, `>$dir/out` are one word
meaning what they mean inside `"…"`, while any quote touching anything
(`'a'"b"`, `--prefix='/opt/my dir'`, `1'2'`) is an error with the canonical
spelling as the hint. The bare-word scanner becomes the string scanner with
different terminators and escapes off. This is the strongest alternative: it
admits the most common sh spelling and none of the partial-quoting ones. It
was declined because it is *typed atoms plus unquoted interpolation* with
rules the spec would have to state one by one (below), because it admits only
part of the sh habit (`--prefix="$dir"` still fails, so whole-word quoting
must still be learned, from fewer spellings), and because its justifying
analogy — `a` already means `"a"` — is false already: `echo hi` runs a
command and `"echo" hi` is a type error, `007` is a number and `"007"` is
text.

**Quotes and `$` ordinary inside a word.** The simplest rule of all: splices
and quotes are recognised only at word start, so `--prefix=$d` is the literal
bare word `--prefix=$d`. It fails silently in exactly the same situations,
with nothing for a diagnostic to hook on. Dominated.

**An explicit string-only concatenation operator.** Operands must be strings,
nothing is implicitly rendered. Clean, and redundant: interpolation is that
operation.

**Hybrids** — glue only after `=`, only when one fragment is literal, only
for `--flag=` — keep the quoting dispute and add exceptions. Refused on sight.

## Footguns

Every case below was found while weighing the alternatives. Each is either
an argument against gluing, a rule a gluing design would have to state, or a
sharp edge of the chosen rule that its diagnostic must respect.

*Silent today, loud under the rule.* `--prefix=$d` → `[--prefix=] [x y]`;
`--prefix='/opt/my dir'` → `[--prefix=] [/opt/my dir]`; `$p/x` with
`p = "/tmp"` → `[/tmp] [/x]`; `$n.txt` → `[7] [.txt]`; `'a'"b"'c'` → three
arguments. `foo[1]` was already loud: an index attaches to the one word
before it, and a literal has nothing to index (T0063).

*Greedy identifiers.* `-` is an identifier character, so `$n-1`, `$dir-old`,
`$name-v2` look up `n-1`, `dir-old`, `name-v2`. True inside `"…"` today and
under every word rule; the undefined-variable error should suggest `$(n)-1`.

*Negation.* Under any gluing rule `-$n` is the text `-7`, not an Int, and
negation is `$[-$n]`; `let m = -$n` type-checks as a String and fails only
when used arithmetically. Under the chosen rule it is an error at the parse.

*An empty splice is not neutral.* Under gluing, `007$empty` is the string
`007` while `007` is the number 7, and `$xs$empty` with a list is a type
error. "Appending nothing" changes type and admissibility.

*Spelling or value.* A numeral fragment glued as source text keeps its
spelling (`007'x'` → `007x`) while a bound numeral glues as its value
(`let n = 007; $n'x'` → `7x`). Numeral classification would have to
distinguish whole bare words from fragments. See
[[invariants/numerals-denote-numbers|numerals-denote-numbers]].

*Missing separators become programs.* `printf '%s\n'"$x"` is one format
argument; `$x$y` is one string where two arguments were meant; the only
gluing case that is a genuine convenience, `'$HOME/'$leaf`, is
`"\$HOME/$leaf"` here.

*Where a word starts, a comment starts.* `#` begins a comment only at the
start of a word. Under a rule where `$x#note` is one word, it prints
`v#note`; today it prints `v`. Changing the word rule moves the comment
boundary.

*`!` changes role.* `!` ends a bare word today, so `foo!bar` is `foo` and
a force of `bar`. A rule that scans bare words as strings makes it the text
`foo!bar` and `a!$b` a splice: `Hello!` would work, and an existing program
would silently change meaning.

*Indexing inside a string.* Outside a string `!{f}[k]` indexes; inside,
§4.2 says `"!{f}[k]"` is the forced value followed by the text `[k]`, and
only `$name` and `!$name` continue into `[k]`. Any rule that scans bare words
as strings must say which of the two `$h[dir]/file` follows. This discrepancy
predates the question and is independent of it.

*Command heads.* A string is not invocable (`"$d/printf" hi` is T0011), so a
glued head `$cmd/bin hi` would fail the same way. Fine, but it means gluing
does not deliver the sh habit for computed heads either, and nobody should
try to recover the bare/path/value head classification of §6.2 by inspecting
the assembled string.

*Nested quotes.* "A quote is a boundary" can only hold at the outer level:
`x=$['a b']` is one word, the inner quote belonging to the splice. One more
rule to state.

*The hint and backslashes.* A bare word has no escapes and an interpolation
has, so the suggested `"…"` spelling is assembled from parsed atoms with
backslashes escaped, never by quoting the source text.

*`a\ b`.* Two words, by the backslash-is-ordinary rule that keeps Windows
paths whole. Not an error, and not what sh means. Documented, not fixable
without reintroducing an escape character.

## The `-c` door

A login shell's `-c` receives whatever sshd's clients send — `git-upload-pack
'it'\''s'`, rsync and scp invocations, `ssh host cmd` — and that is sh:
`'it'\''s'`, but also `&&`, `2>&1`, `$HOME`, `"$@"`, backticks. Gluing quotes
would make the apostrophe case work and leave everything else broken, which
is the least predictable outcome of all. No word rule short of being sh fixes
this; it is a decision about the `-c` door (forward non-tty `-c` to
`/bin/sh`, as `ral-sh` does, or document that ral does not accept sh over
ssh, as fish does), and it is orthogonal to how ral's own words are read.

## Where it lives

The atom list is `is_bare_char` and the quote and splice scanners in
`core/src/syntax/lexer.rs`; the word rule is checked where adjacency is
already read, beside the `$name[k]` and `!{f}[k]` productions in the parser;
the diagnostic is `P0002`, with a two-reading help. Spec
§3.5 states the rule after "Quote a word when there is any doubt"; `ral.md`
no longer needs to tell the agent that a composite path is one quoted word,
because the parser tells it, with the spelling.
