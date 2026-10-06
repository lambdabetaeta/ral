---
status: accepted
generated_at_commit: 8d868e18
---

# A boundary is checked against its solved type

**A door through which a value of a shape the program did not decide enters
typed code admits that value against the type the checker solved at its
call.** It admits; it never converts. What fails is the decode, naming the
field and the line that needed it.

## What was decided

- **The set is closed.** The decoders of undecided shape (`from-json`,
  `from-jsonl`, and the new `from-json-at`), `use`, `service-handle`, every tag
  of `exarch-agents`, `exarch-pins` and `exarch-transcript`, and `_ed-state`,
  whose cell persists across sessions. A `BuiltinEntry` marks itself with
  `BuiltinEntry::boundary`, and its body is a `BuiltinBody::Boundary`, which is
  handed the site. `from-csv` decides its shape (`[Map String]`) and is not
  one; nor is `detach`, whose receipt has one shape; nor `$ENV`, typed
  `Map String`.
- **The site is on the call.** `Exec` carries `site: Option<Arc<Site>>`, filled
  by `annotate` for a boundary head. There is no `Check` frame: the door
  already knows the value, and a frame sees it only after the door has let it
  go. `serial.rs` ships the site with the IR, so an engine child admits too.
  The checker builds the site (`typecheck/site.rs`); the runtime walks it
  (`types/admit.rs`).
- **A `Site` is the solved type frozen as a graph**, one node per variable
  root, so cycles close and sharing survives; it keeps the span that imposed
  each structure and each free variable's kind. `Site::admit` walks the value
  and the graph together. Its rules:
  - a ground scalar position needs that scalar's head, and a JSON `3` is an
    `Int` and `2.5` a `Float` at a ground site as under a variable: a door
    never reads `3` as `3.0`, and `float` at the use is the one word that
    accepts both;
  - a free variable is checked against its kind alone, at every occurrence; the
    one exception is a `comparable` variable, *fixed* at its first scalar to
    numbers or to text, because ordering a number against text is the one
    operation that fails on two admissible values of different types
    (`Fixings`, one per unit, shared by all its sites);
  - an open row checks the fields it names and its tail's deep bit the rest; a
    closed row requires the exact field set, so a decoded value joined with a
    closed record literal refuses an extra field rather than dropping it;
  - a variant needs a tag of its row, and a decoder produces none;
  - a block position needs a closure, and `Site::admit_module` holds a `use`
    export to its scheme in one scratch unifier over the whole record.
- **The report is the pointer.** A mismatch names an RFC 6901 pointer into the
  value, what was found, what the script uses it as, and the line of the use
  that imposed it, with a secondary caret on that use.
- **A boundary used as a value is η-expanded.** `$from-json` becomes
  `{ from-json }`, so the block holds a saturated call that carries a site; so
  does an under-applied call, wherever it stands, a block tail included.
- **Sessions.** A residual weak variable stays weak across units. A data
  binding that enters a unit with residuals is admitted again, against the
  type the unit solved, before the unit runs (`Toplevel.admits`); a thunk's
  results do not exist yet and are admitted by the sites in its own IR.
- **`from-json-at tokens`** walks reference tokens (an object member by exact
  key, an array element by canonical decimal), so a path needs no escaping and
  a miss names the pointer prefix and the keys or the length.
- **The perimeter is a test.** No builtin outside the boundary set and the
  divergent `fail`/`exit`/`quit` has a variable only its result mentions, and
  no stored `Define` scheme quantifies what a boundary decides. `detach`'s
  `∀α` is listed as the one known exception until it is typed.

## Why

HM cannot know a decoded value, but the checker does know what the program
will do with it. Checking at the door turns "fails later, elsewhere" into
"fails here, naming the field and the line that needed it". For exarch that
matters most: `$r[reply][verdict]` on a child's reply fails at the `` `read ``,
saying `/reply/verdict` is missing.

## Rejected

Declaring the casts, which leaves them unsound. A JSON sum type, which is
verbose and is the reason `from-json-at` exists. A `Check` frame wrapping the
call: the door knows what the frame has to be told. Reading a JSON integer as a
`Float` where the type says so: a decoder that converts by type is a second
decoder. Fixing every free variable at its first scalar: it refused
`for $xs { |x| echo $x }` over `[1, "a"]`, which is parametric.

Builds on [[decisions/260930_a-let-generalises-what-is-not-weak|a-let-generalises-what-is-not-weak]],
whose weak variables are what make the type recorded at a call the one every
use of the value fixes; uses [[decisions/260930_operators-are-kinded|kinds]]
for the free-variable check.
