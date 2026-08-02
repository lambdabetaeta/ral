<!-- verified_at_commit: 001c8410 -->
# ral(1) — language specification

## 1. About this specification

This document defines the observable behaviour of ral programs and the `ral`
shell. It is a reference. The tutorial teaches the language step by step.

The specification covers three surfaces:

- the core language, which every ral host must provide;
- the standard surface provided by the `ral` program;
- features that depend on a host or an operating system.

A host can add builtins. Such a builtin is part of that host, not part of the
portable core. A platform note changes only the rule that it names. All other
rules still apply.

This document specifies results that a program can observe. It does not specify
Rust modules, internal process layouts, or implementation algorithms. The code
is the authority for the current implementation.

### 1.1. Normative terms

The words **must** and **must not** state requirements. **May** states a permitted
choice. “Is an error” means that ral must reject the program or operation. A
rule states the rejection phase when that phase is observable.

Examples show the rules but do not replace them. The formal definition is
authoritative for grammar and types. The plain-language sections are
authoritative for observable runtime behaviour.

### 1.2. Reading paths

Read the core model first. Then read the sections on values, bindings, commands,
pipelines, and failure to write ordinary programs. Read the later sections when
you need modules, concurrency, capabilities, the interactive shell, or platform
integration. The final formal section is for implementers and readers who want
the type and evaluation rules.

## 2. The core model

ral keeps data separate from work. This distinction explains most of the
language.

- A **value** is data. Strings, numbers, lists, records, and blocks are values.
- A **command** is work. It can return a value, write bytes, change permitted
  shell state, or fail.

ral checks how values and commands fit together before it starts the program.
Most code does not need type annotations.

### 2.1. Values do not run by themselves

A quoted string, a number, or a collection is already a value:

```ral
'hello'
42
[host: 'example.net', port: 443]
```

`let` runs its right-hand side and binds the result:

```ral
let answer = 42
let host = hostname
```

The first line binds the integer `42`. The second line runs `hostname` and
binds its text result. Bindings are immutable. A later `let` can shadow an
earlier binding for the rest of its scope.

Use `$` when you want the value bound to a name:

```ral
echo $host
```

Without `$`, a word in argument position is text. For example, `echo host`
passes the string `"host"` to `echo`.

### 2.2. Blocks make work into values

A block stores a command without running it:

```ral
let now = { date +%s }
```

Use `!` to run stored work where a value is needed:

```ral
let timestamp = !$now
let inline = !{date +%s}
```

A block can take parameters:

```ral
let greet = { |name| "hello, $name" }
let message = greet 'world'
```

In command position, ral recognizes the bound name `greet` and applies the
block. In other positions, use `$greet` to pass the block itself as data.
Blocks remember the bindings that were visible when they were created.

### 2.3. Commands have two results

A command can produce two independent things:

1. A return value for ral.
2. A stream of bytes for a terminal, file, or pipeline.

An external command usually writes bytes. On the right-hand side of `let`, ral
captures the final byte output and decodes it as UTF-8 text:

```ral
let branch = git branch --show-current
```

If the bytes are not UTF-8, decode them explicitly or keep them as `Bytes`.
A command that returns another value binds that value directly.

The `|` decides what passes between two stages. A byte pipe passes the byte
output and discards the first stage's return value. A value pipe passes the
first stage's return value.

### 2.4. Pipelines carry bytes or values

A pipeline uses one kind of pipe at each `|`.

- A **byte pipe** streams bytes, as in a Unix pipeline:

  ```ral
  printf 'one\ntwo\n' | wc -l
  ```

- A **value pipe** passes a value as the next command's final argument:

  ```ral
  [1, 2, 3] | length
  ```

ral determines the pipe kind from the commands on both sides. It reports
an error before execution if the two sides do not agree. Codecs such as
`from-json` and `to-json` cross between bytes and structured values
explicitly.

### 2.5. Failure is not false

A command either succeeds or fails. An unhandled failure stops the remaining
commands. `?` runs a fallback only after failure:

```ral
cat 'optional.conf' ? echo 'default'
```

A Boolean is ordinary data. Returning `false` is still a successful command,
so it does not select the right side of `?`. Use `if` to branch on a Boolean,
and use `try` when code must inspect or recover from a failure.

### 2.6. A session remembers; a block is contained

At the session level, completed changes remain available to the next run. This
includes bindings and the shell's current directory. Changes made before a
top-level failure remain too.

A block is a boundary. Bindings, directory changes, module loads, and similar
shell-state changes made inside it do not escape when it returns. The block's
value, output, failure, and recorded observations can cross the boundary.

Some settings follow calls instead of being captured by blocks. `within`
temporarily changes the directory, environment, or command handlers. `grant`
temporarily reduces authority. Code called inside either scope sees that scope,
even if the code was defined elsewhere.

### 2.7. One complete example

```ral
let salutation = 'hello'
let greet = { |name| "$salutation, $name" }
let message = greet 'world'
echo $message | tr a-z A-Z
```

This program does the following:

1. It binds a string value to `salutation`.
2. It binds a parameterized block to `greet`. The block remembers
   `salutation`.
3. It applies `greet` and binds the returned string to `message`.
4. It reads `$message`, writes it as bytes with `echo`, and streams those bytes
   through the external command `tr`.

The output is:

```text
HELLO, WORLD
```

The following sections define each of these rules precisely.

## 3. Source text and program structure

A ral program is a sequence of statements. A script can use a newline or a
semicolon (`;`) to separate statements.

```ral
echo 'first'
echo 'second'; echo 'third'
```

An empty program does nothing and succeeds. Blank lines and extra semicolons
between statements have no effect.

### 3.1. Source text

Source files must contain valid UTF-8 text. When ral loads a script or module,
it converts Windows line endings (`CRLF`) and old-style `CR` line endings to
`LF`. These files therefore have the same meaning on every platform.

Spaces and tabs separate words. They do not separate statements. A newline
usually ends the current statement. A semicolon always ends it.

There is no general backslash continuation. A backslash outside a
double-quoted string is an ordinary character. Double-quoted strings have
their own escape and line rules; see *Strings*.

### 3.2. Comments

`#` starts a comment when it occurs where a new word could start. The comment
ends at the next newline or at the end of the source.

```ral
# A complete comment line.
echo 'ready'  # A comment after a command.
curl http://example.test/page#part
```

The last `#` is part of the URL. It does not start a comment because it occurs
inside a word.

A run of `#` characters followed immediately by `'` starts a raw string, not
a comment. For example, `#'can't'#` is one string. See *Strings* for the full
rule.

ral has no block comments. A comment does not cancel an open delimiter. In an
interactive session, ral asks for more input when a comment follows an open
`{` or `[`. In a complete script, an unclosed delimiter is an error.

A script can start with a conventional interpreter line:

```ral
#!/usr/bin/env ral
```

ral reads this line as a comment.

### 3.3. Newlines and continuation

A newline can continue a statement only in the cases in this section. A
semicolon cannot continue a statement in any of these cases.

#### Pipelines

`|` can occur before or after a newline. Blank lines and comment lines are
also allowed at that point.

```ral
generate
| transform |
  consume
```

These three lines form one pipeline. The following text is an error because a
semicolon ends the first statement:

```ral
generate; | consume
```

#### Failure chains

`?` can occur at the start of the line after the command that it follows. The
next branch must start on the same line as `?`.

```ral
read-primary
? read-backup
```

Do not put a newline after `?`:

```ral
read-primary ?
read-backup       # error
```

#### Bindings and control forms

The right-hand side of a `let` binding can start on a later line when the
newline follows `=`.

```ral
let report =
  build-report
```

The condition and body of `if` and `elsif` can start on later lines. `elsif`
and `else` can also start on the line after the preceding branch. The two
operands of `case` can be separated by newlines.

```ral
if
  $ready
  { deploy }
else
  { explain-delay }

case
  $result
  [`ok: { |value| use $value }, `err: { |error| fail $error }]
```

A newline after `return` ends that statement. It does not attach the value on
the next line.

```ral
return
42          # a new statement
```

Command arguments do not continue onto the next line merely because the next
line is indented. Bind a long argument list to a value and spread it when you
need to split such a command across source lines.

### 3.4. Braces, brackets, and parentheses

Braces (`{ ... }`) contain a block. Newlines inside a block still separate
statements.

Brackets (`[ ... ]`) contain a collection, pattern, index, or expression
block. Newlines are whitespace while the bracket is the innermost open
delimiter. Collection entries still need commas; a newline does not replace a
comma.

```ral
let ports = [
  8080,
  8081,
]

let prompt = [render: {
  let mark = '>'
  return $mark
}]
```

The block in the second example is inside a map, but its own braces are the
innermost delimiters. Its newline therefore separates the `let` and `return`
statements.

Parentheses group expressions inside `$[...]` and delimit names in
`$(name)`. They are not a general command-grouping form.

Postfix indexing requires no space before `[`. Thus `$item[key]` is one
indexed value, while `$item [key]` is two values in a command.

ral reports mismatched or unclosed delimiters. It also rejects source with
more than 64 nested forms. This limit prevents an implementation stack
overflow.

### 3.5. Words and punctuation

A bare word is source text that needs no quotes. Most visible characters can
occur in a bare word. Spaces, tabs, line endings, and these characters end a
bare word or start another language form:

```text
|  {  }  [  ]  $  ^  !  ~  <  >  "  '  `  (  )  ;
```

Some characters depend on their position:

- `#` starts a comment only at the start of a word. It remains literal inside
  a word.
- `,` separates items while `[...]` is the innermost form. Outside brackets,
  it can be part of a word, as in `--features=a,b`.
- `:` is punctuation before whitespace, a newline, `]`, or the end of input.
  It remains part of forms such as `localhost:5432`.
- `?` and `&` are operators where a new word can start. They can occur inside
  a longer bare word.
- `...` is the spread marker where a new word can start. It can occur inside
  a longer word.

Quote a word when there is any doubt. Quoting prevents punctuation in its
contents from changing the program structure.

Backslash has no special meaning in a bare word. For example,
`C:\Users\name` is one bare word. A dot is also ordinary, so `.env` and
`archive.tar` are bare words.

### 3.6. Names and reserved words

A binding name starts with an ASCII letter or `_`. Later characters can be
ASCII letters, digits, `_`, or `-`. Names are case-sensitive.

These words cannot be binding names or block parameters:

```text
if  elsif  else  let  return  case
within  grant  try  guard  audit
true  false  unit
```

This restriction is contextual. For example, a quoted occurrence is always
string data, and `^name` uses an external command name rather than a binding.

### 3.7. Statements and stages

A statement is either a `let` binding or a command chain. A command chain can
contain pipelines joined by `?`. A pipeline contains one or more stages joined
by `|`.

`let` is a statement form. It cannot occur after `|` or `?`.

```ral
let result = produce | consume

produce | let result = consume   # error
produce ? let result = fallback  # error
```

A trailing `&` belongs to the pipeline before it. Its concurrency behavior is
defined in *Concurrent work*. On a `let` right-hand side, one final `&`
applies to the complete failure chain. Individual branches of that chain
cannot carry their own `&`.

The complete formal grammar is in §17.

## 4. Values and data

A value is data. ral does not split it into words or read it again as source
text. For example, a string that contains spaces remains one value and one
command argument.

ral has these kinds of value:

| Kind | Meaning |
|---|---|
| `Unit` | One value that carries no information |
| `Bool` | `true` or `false` |
| `Int` | A signed 64-bit integer |
| `Float` | A finite 64-bit floating-point number |
| `String` | Unicode text |
| `Bytes` | An arbitrary finite sequence of bytes |
| `List` | An ordered sequence of values |
| record or map | Values stored under string keys |
| variant | A tag with an optional payload |
| block or function | A suspended command |
| handle | A reference to concurrent work |

Blocks, functions, and handles are values, but later sections define how to
run or inspect them.

### 4.1. Scalar literals

The literals `unit`, `true`, and `false` produce `Unit` and `Bool` values.
Decimal integer literals produce `Int` values. A floating-point literal must
contain a decimal point. It may also contain an exponent.

```ral
return unit
return true
return 42
return -7
return 3.14
return 1.0e6
```

An unquoted word in value position is otherwise a `String`. Thus `return
hello` returns the text `hello`, and `return 1e6` returns text because that
spelling has no decimal point. A numeric spelling that is outside the range of
its value kind is also an ordinary word.

Every `Float` is finite. A float literal that would produce infinity is not a
float literal. The `float` conversion rejects NaN and positive or negative
infinity. Floating-point arithmetic fails if its result would be non-finite.

### 4.2. Strings

A `String` contains valid UTF-8 text. `length` counts Unicode characters, not
UTF-8 bytes.

Single quotes make literal text. They do not process escapes or interpolation:

```ral
return 'literal $name and \n'
```

Add matching `#` fences when the text contains a single quote. Add more `#`
characters when the text contains a possible closing fence:

```ral
return #'it's literal'#
return ##'body with '# inside'##
```

Literal strings may span lines. Their contents are exact after source-text
normalization. A line ending written as `CRLF` or a lone `CR` in loaded source
therefore appears as `LF` in the value.

Double quotes process escapes and interpolation:

```ral
return "hello $name"
return "field: $record[key]"
return "host: !{hostname | from-line}"
return "sum: $[2 + 3]"
```

`$(name)` marks the exact end of a name when the following text could be part
of it. The supported escapes are `\n`, `\r`, `\t`, `\\`, `\0`, `\e`, `\"`,
`\$`, `\!`, `\xNN`, `\u{X..}`, and backslash followed by a line ending.
`\xNN` accepts one ASCII byte from `00` to `7F`. `\u{X..}` accepts one valid
Unicode scalar value written with one to six hexadecimal digits. An unknown
or malformed escape is an error.

Interpolation accepts `Unit`, `Bool`, `Int`, `Float`, and `String`. `Unit`
becomes empty text. The other scalar values use their normal text form.
Interpolation rejects bytes, collections, variants, blocks, functions, and
handles. Use an explicit conversion or select a field instead.

### 4.3. Bytes

`Bytes` can contain any byte, including NUL and invalid UTF-8. There is no byte
literal. Codecs and input operations create byte values. `to-bytes` also
accepts a list of integers from 0 to 255. Section 7 defines byte pipes and the
codecs that cross between bytes and values.

`length` counts bytes. Equality compares bytes exactly.

Displaying bytes decodes them with replacement characters for invalid UTF-8.
This display is only for inspection; it can lose information. `from-string`
performs a strict UTF-8 decode and fails on invalid input.

### 4.4. Lists

A list is ordered. Its elements must have compatible types. Use commas between
elements. A trailing comma is allowed.

```ral
let names = ['Ada', 'Grace', 'Edsger']
let empty = []
```

`...` inserts the elements of another list at that position. It creates a new
list and does not change the source list.

```ral
let middle = [2, 3]
return [1, ...$middle, 4]
```

Spreading a value that is not a list is an error.

### 4.5. Records and maps

Records and maps use the same runtime value: a string-keyed collection. They
differ in what the checker knows.

- A record has known fields. Its fields may hold different kinds of value.
- A map has keys computed at runtime. Its values must have compatible types.

```ral
let server = [host: 'db.example', port: 5432]
let key = 'primary'
let ports = [$key: 5432]
let empty_map = [:]
```

A static key can be a bare name or a quoted string. A computed key uses a
string value such as `$key`. Any computed key that is not a `String` is an
error. Keys are unique in the resulting value and iterate in sorted order.

A duplicate explicit key produces a warning. The last explicit entry wins.
Map spreading has these priority rules:

1. Every explicit entry wins over every spread entry, regardless of position.
2. If spread entries conflict with each other, the first spread entry wins.

```ral
let defaults = [host: 'localhost', port: 80]
let server = [...$defaults, port: 8080]
```

Spreading a value that is not a record or map is an error. Spreading does not
change the source value.

A tag can also be a static record key:

```ral
return [`development: 8080, `production: 443]
```

One record literal cannot mix ordinary static keys and tag keys. This rule
keeps ordinary records separate from the handler tables used with variants.

### 4.6. Variants

A variant records one named outcome. It can carry no payload or one value:

```ral
let absent = `none
let answer = `ok 42
let problem = `error [message: 'not found']
```

The tag takes the next value atom as its payload. A statement boundary, comma,
closing bracket, pipe, `?`, redirect, or `&` leaves it without a payload.
Section 8 defines `case`, which selects a handler by tag.

### 4.7. Indexing

Indexing is postfix syntax. The `[` must immediately follow the value being
indexed.

```ral
$items[0]
$server[host]
$config[database][port]
```

Lists use zero-based, non-negative `Int` indices. Records and maps use `String`
keys. A missing key, an out-of-range list index, a key of the wrong kind, or an
attempt to index another value kind is an error.

### 4.8. Equality and ordering

`equal a b` and `==` or `!=` inside `$[...]` use the same structural equality.

- Unit, booleans, strings, and bytes compare by value.
- Numbers compare numerically. Comparing an `Int` with a `Float` converts the
  integer to floating point first.
- Lists compare their elements in order.
- Records and maps compare their key-value pairs without regard to insertion
  order.
- Variants compare their tag and payload.
- Values of different, unrelated kinds are not equal.

Blocks, functions, and handles have no equality operation. Comparing one is an
error, including when it is nested in a collection.

Ordering is defined only for numbers and strings. Integer-to-integer ordering
keeps the full 64-bit integer precision. String ordering is lexicographic.

### 4.9. Display

ral has a text conversion for command output and a value renderer for
interactive hosts. They serve different purposes.

Text conversion writes strings without quotes, writes `Unit` as empty text,
and writes scalar values in their usual form. It shows lists, records, maps,
and variants with brackets and tags. It shows bytes as lossy UTF-8. This form
is readable but is not always valid ral source and is not a serialization
format.

The interactive value renderer quotes strings with a safe hash fence, writes
`unit` for `Unit`, and lays out collections to fit the available width. Maps
display in sorted key order. A host may shorten deeply nested collections or a
long string stored directly under a map key. It must mark any omitted content.
Lists and top-level strings are not shortened by the standard `ral` renderer.

## 5. Bindings, patterns, blocks, and functions

`let` gives a name to the result of a command or to a value.

```ral
let branch = git branch --show-current
let port = 8080
let greeting = 'hello'
```

The first binding runs `git` and binds its result. The other two bind values
without running an external command. Use `$name` to read a binding outside
command-head position.

```ral
echo "$greeting from port $port"
```

A bare name in command-head position follows the lookup rules in *Commands
and name lookup*. This is why `let branch = git ...` runs `git`, while
`let greeting = 'hello'` binds a quoted value.

### 5.1. Immutable bindings and scope

A binding cannot be changed. A later `let` with the same name creates a new
binding that hides the old one in the current scope.

```ral
let colour = 'blue'
let colour = 'green'
echo $colour                 # green
```

Bindings use lexical scope. Code uses the bindings that were visible where
that code was defined.

```ral
let rate = 10
let add-rate = { |n| return $[$n + $rate] }
let rate = 20
add-rate 5                   # 15
```

The function keeps the first `rate`. A binding at its call site cannot change
that captured value.

At session scope, ral refuses a binding whose name is an executable command
on the current `PATH`. It checks the complete pattern before it evaluates the
right-hand side.

```ral
let git = 'not a command'    # error if git is on PATH
```

This rule prevents a top-level value from silently replacing a command.
Parameters and bindings inside blocks can use such a name because they are
local. Prelude functions and builtins can also be hidden, although ral warns
when a binding hides a prelude name.

### 5.2. Patterns

The left-hand side of `let` is a pattern. Function parameters use the same
patterns.

| Pattern | Meaning |
|---|---|
| `name` | Bind the complete value. |
| `_` | Ignore the value. |
| `[first, second]` | Bind a list of exactly that length. |
| `[first, ...rest]` | Bind the first item and bind the remaining list to `rest`. |
| `[host: h, port: p]` | Read named fields from a map or record. |
| `[port: p = 8080]` | Use `8080` when `port` is absent. |

Patterns can be nested.

```ral
let [first, ...rest] = [10, 20, 30]
let [name: user, address: [city: city]] = $account
```

One pattern cannot bind the same name twice. ral reports this as a parse
error, including when the repeated name is nested or used as a list tail.

A list pattern without `...rest` must match the complete list. It is an error
if the list has too few or too many items. A map pattern requires every field
that has no default. Extra map fields are allowed.

A default is evaluated only when its field is absent. It uses the lexical
scope of the pattern.

```ral
let default-port = { return 8080 }
let [host: host, port: port = !{default-port}] = [host: 'localhost']
```

A pattern either binds every name or binds none of them. If any nested part
does not match, ral reports an error and leaves the scope unchanged. `try` can
catch this runtime error.

Patterns are structural. They cannot test for a literal value. Use `if` for a
Boolean decision and `case` for a variant.

### 5.3. Plain blocks

A plain block stores commands as a value. Creating it does not run its body.

```ral
let announce = { echo 'ready' }
```

Force a block with `!` to run it:

```ral
!$announce
!{echo 'run this anonymous block'}
```

`!` means force. It is not Boolean negation. Use `not` for Boolean negation
inside `$[...]`.

A bound plain block is also forced when its bare name is the command head.
These two statements are equivalent:

```ral
announce
!$announce
```

A block captures lexical bindings when the block is created. Its body runs in
a fresh local scope. Bindings created by the body do not escape. Changes to
the current directory and other block-local shell state also do not escape.
The block's output, result, failure, audit records, and final status remain
observable.

```ral
let outside = 'outer'
let result = !{
  let outside = 'inner'
  let local = 42
  return $outside
}

echo $result                  # inner
echo $outside                 # outer
echo $local                   # error: no such binding
```

An empty block returns `unit`. A non-empty block returns the result of its
last statement. Earlier statement results are discarded.

### 5.4. Functions

A parameterised block is a function. Put one or more space-separated patterns
between the two `|` characters.

```ral
let add = { |x y| return $[$x + $y] }
let describe = { |[name: name, port: port]| echo "$name:$port" }
```

Do not put commas between parameters. `{ || ... }` is invalid; use a plain
block `{ ... }` when there are no parameters.

Each parameter is a separate pattern. A later parameter can therefore use the
same name as an earlier parameter. The later binding hides the earlier one in
the remaining function body.

```ral
let keep-second = { |item item| return $item }
keep-second 'first' 'second'  # second
```

Call a function by putting it in command-head position:

```ral
add 2 3                       # 5
describe [name: 'web', port: 443]
```

Use `$function` when passing a function as data.

```ral
let increment = { |n| return $[$n + 1] }
map $increment [1, 2, 3]
```

Without `$`, `increment` in argument position is the string `"increment"`.

Each argument is applied from left to right. If a call supplies too few
arguments, it returns a new function that waits for the remaining arguments.

```ral
let add = { |x y| return $[$x + $y] }
let add-five = add 5
add-five 3                    # 8
```

If arguments remain after the function returns, ral applies them to the
returned value. This is valid when that value is another function. Otherwise,
ral reports a type error or a runtime error.

A function call gets a fresh local binding scope. Its parameters and local
`let` bindings do not escape. The function still acts in the caller's shell:
for example, a successful `cd` in a function changes the caller's current
directory. This differs from forcing a plain block, which discards its current
directory change.

### 5.5. `return`

`return value` produces `value` as the successful result of the current
statement. `return` with no value produces `unit`. It accepts at most one
value.

Despite its name, `return` is not an early-exit statement. It does not skip
later statements in a block, function, or file.

```ral
let example = {
  return 1
  return 2
}

!$example                     # 2
```

Use `if`, `case`, or a failure to choose which computation runs. ral has no
non-local early return.

### 5.6. Top-level runs and local bodies

A top-level run is one complete script, one `-c` program, one submitted REPL
entry, or one exarch tool call. When the host keeps the session open,
successful top-level bindings are available to the next top-level run.

If a top-level run fails, effects completed before the failure still remain.
A binding after the failing statement is never evaluated.

```ral
let before = 1
failing-command
let after = 2
```

After this failed top-level run, `before` remains bound in the session and
`after` does not exist.

Forced plain blocks are local bodies. The bodies used by `within`, `grant`,
`try`, `guard`, and `audit` follow the same local rule. Their bindings do not
become top-level session bindings.

### 5.7. Recursion and forward references

A simple named block binding can refer to itself.

```ral
let countdown = { |n|
  if $[$n == 0] { return unit }
  else { countdown $[$n - 1] }
}
```

Named blocks in the same statement list can also refer to each other in any
source order. This supports mutual recursion and helper functions written
after their callers.

```ral
let even = { |n|
  if $[$n == 0] { return true }
  else { odd $[$n - 1] }
}

let odd = { |n|
  if $[$n == 0] { return false }
  else { even $[$n - 1] }
}
```

Forward references apply only to simple named bindings whose right-hand side
is a plain or parameterised block. A value must still be bound before an
earlier statement can read it.

When a name is defined more than once, a reference inside such a block uses
the nearest preceding definition. If there is no preceding block definition,
it can use the first later block definition in the same statement list. This
keeps ordinary shadowing while allowing forward block references.

Tail-recursive calls reuse the current evaluator frame. Non-tail recursion is
limited by the configured recursion limit and fails with a clear error if it
exceeds that limit.

The formal account of function application, recursion, and inferred types is
in §17.

## 6. Commands and name resolution

A command starts with a **head**. The head decides whether ral applies a value,
invokes a handler, or starts a program. Arguments do not use command lookup.

### 6.1. Command and value contexts

A statement and the right-hand side of `let` are command contexts:

```ral
hostname
let host = hostname
```

Both lines run `hostname`. The second line binds its result.

Arguments, `return` operands, collection elements, and interpolations are value
contexts:

```ral
echo hostname
return hostname
let tools = [hostname, uname]
```

Here, `hostname` and `uname` are strings. Use `$` to read a binding:

```ral
echo $host
return $host
```

A value written alone in command context returns itself. This lets a literal or
collection be the result of a block:

```ral
{ 'ready' }
{ [host: 'example.net', port: 443] }
```

### 6.2. Head forms

The spelling of the head selects one of four paths.

| Form | Examples | Meaning |
|---|---|---|
| Bare head | `git status`, `map $f $xs` | Use the lookup order in §6.3. |
| Path head | `./build`, `/usr/bin/env`, `~/bin/tool` | Run that path directly. |
| Explicit value head | `$f 3`, `!$factory 3` | Apply the value. Never search for a program. |
| Caret head | `^git status` | Skip value bindings, then continue through handlers and program lookup. |

A path head does not consult bindings, handlers, bundled commands, or `PATH`.
ral expands `~` when it resolves the command. A relative path uses ral's current
logical directory.

An explicit value head stays in the value world. If it is not callable, ral
reports an error. It does not reinterpret the value as a command name.

A bare `$f` with no arguments is only a value:

```ral
let copy = $f       # store f
let result = f 3    # apply f
let same = $f 3     # apply f explicitly
```

`^name` is valid only at the start of a command. Its operand must be a plain
name, not a path. The caret skips user bindings, prelude functions, and
fixed-arity builtins. It still respects named handlers, base handlers,
catch-all handlers, and active capabilities.

For example, `^echo` still reaches a user handler for `echo`, or ral's base
`echo` handler when no user handler matches. Use an exact path to bypass all
handlers.

The bare names `within`, `grant`, `try`, `guard`, and `audit` are reserved
control forms. A caret form such as `^try` uses caret lookup instead.

### 6.3. Bare-head lookup

ral resolves an ordinary bare head in this order. The first match wins.

1. **A value binding.** ral searches the current lexical scopes, session
   bindings, and prelude. Fixed-arity builtins are callable values in this
   namespace. A matching value must be callable. Otherwise, lookup fails
   without falling through.
2. **A named user handler.** This includes persistent handlers installed by
   `alias` and scoped handlers installed by `within [handlers: …]`. The
   innermost handler for the name wins.
3. **A base handler.** Builtins without fixed surface arity live here. `echo`
   is the standard example.
4. **A catch-all handler.** `within [handler: …]` handles the call only if no
   binding, named handler, or base handler claimed it.
5. **A bundled or external command.** A bundled command wins when the current
   build supplies that bare name. Otherwise, ral searches the effective
   `PATH`.

Prelude functions behave like user functions: use their bare name as a head,
and `$name` when passing one as a value.

Fixed-arity builtins are callable values. They support partial application and
higher-order use when their interface has a value form. A command-only builtin
reports an error when used through `$name`.

A named handler may share a name with a binding or fixed-arity builtin. The
ordinary bare name still selects the binding. `^name` skips the binding and
reaches the handler. A named handler for a base handler such as `echo` already
has higher priority, so it does not need a caret.

### 6.4. Handlers and aliases

A named handler or alias takes one parameter. ral passes the arguments after
the head as one list:

```ral
within [handlers: [deploy: { |args| audit-deploy $args }]] {
    deploy 'prod' '--wait'
}
```

A catch-all handler takes the dispatched name and the argument list:

```ral
within [handler: { |name args| log-call $name $args }] {
    unknown-command 'arg'
}
```

ral checks handler arity when the handler is installed. It also checks that the
handler preserves the command's byte-pipe and value-pipe behavior. A handler
for `echo`, for example, must still produce byte output.

Handlers are self-masking. While a selected handler runs, its own frame is
temporarily absent. A call to the same name from its body reaches an outer
handler, a base handler, a catch-all handler, or an external command:

```ral
within [handlers: [git: { |args|
    echo 'running git' 1>&2
    git ...$args
}]] {
    git status
}
```

Any named handler takes priority over any catch-all handler, even when the
catch-all is in a more deeply nested scope. A base handler also takes priority
over every catch-all handler.

### 6.5. Application and arity

Parameterized blocks and fixed-arity builtins are curried:

```ral
let add = { |a b| $[$a + $b] }
let add-two = add 2
let five = add-two 3
```

Too few arguments return a partially applied callable. The exact number runs
the body. If arguments remain after the call returns, ral tries to apply them
to the returned value. It reports an arity error when that value is not
callable.

ral checks known arity and argument-type errors before execution. A spread can
make the final arity unknown until runtime.

### 6.6. Arguments and spreading

Command arguments are values. Ordinary and quoted words produce strings.
Numeric and Boolean literals keep their types. `$name` supplies a bound value,
and `!{…}` supplies a forced block's result.

`...value` spreads a list into positional arguments:

```ral
let flags = ['--all', '--long']
ls ...$flags '/tmp'
```

The spread value must be a list. Its elements are inserted in order and remain
ral values when passed to a function, builtin, or handler.

Operating-system arguments must become text. ral formats strings, integers,
floats, Booleans, `Unit`, and tagged values. `Unit` becomes an empty argument.
ral rejects bytes, lists, maps, blocks, functions, native callables, and
handles. Spread a list, and send bytes through a byte pipe or decode them
first.

Arguments are never split, globbed, or parsed again. A string containing spaces
remains one argument.

### 6.7. External commands

A bare external name uses the effective `PATH`. A `within [env: [PATH: …]]`
scope changes that search for its body. ral resolves the command once, then
starts that program with the current logical directory, environment, redirects,
and capability restrictions.

When the build supplies a bundled command with the requested bare name, the
bundled command wins over a same-named file on `PATH`. It still runs as a child
command with the same byte-pipe, failure, audit, and sandbox behavior as an
external program. Use a path head to select a particular system binary.

A missing bare command fails with status 127. A file found on `PATH` but not
executable fails with status 126. A non-zero process result is a failure,
except for the specified SIGPIPE case in a non-final byte-pipe stage.

### 6.8. Returned values and emitted bytes

A command can return a ral value and emit bytes independently.

- In statement position, bytes go to the current output unless redirected. The
  returned value is discarded.
- On the right-hand side of `let`, ral binds the returned value. When the final
  result is byte output without a separate value, ral captures it, decodes it
  as UTF-8, and removes one trailing newline.
- In a byte pipe, bytes pass to the next stage. A non-final stage's returned
  value is discarded.
- In a value pipe, the returned value becomes the next stage's final argument.
- A redirect changes where emitted bytes go. It does not make those bytes the
  returned value.

```ral
let branch = git branch --show-current
let saved = echo 'hello' > greeting.txt
```

`branch` receives the captured branch name. The redirect sends `echo`'s bytes
to `greeting.txt`, so `saved` is `""`.

Captured text is data. ral does not re-lex it, split it, or expand wildcards in
it. Use a decoder for structured bytes, and use `from-bytes` for data that is
not UTF-8.

## 7. Pipelines and input/output

A pipeline connects computations with `|`. The typechecker decides how each connection carries data. A **value pipe** carries a ral value. A **byte pipe** carries bytes, like a Unix pipe.

The choice follows from the stages’ types. ral never guesses by converting a value to text. A mismatch is a type error, reported before any stage starts.

### 7.1. Value pipes and byte pipes

A pipeline made entirely of value pipes is a **value pipeline**. It runs from left to right in the current evaluator. The value from the left becomes the last argument of the stage on the right:

```ral
$items | map $f
# means: map $f $items
```

The producer is evaluated exactly once. A value pipeline does not create processes, use operating-system pipes, or enter job control. Changes to ral state have the same scope as they would outside the pipeline.

If any stage needs bytes, the whole pipeline is **process-staged**. Every stage then runs in a child process, including stages written in ral. Byte pipes use operating-system pipes; value pipes between child stages use ral’s typed value transport. Byte-producing and byte-consuming stages can run at the same time.

State changes made by a process-staged stage are private to that child. Its working directory, environment changes, aliases, loaded modules, and bindings do not alter the parent. Only pipe contents, the final result, and recorded observations cross the process boundary.

An external command cannot receive a structured value by accident. Encode the value before a byte pipe, and decode bytes before a value pipe:

```ral
$record | to-json | jq '.name' | from-line
```

### 7.2. Output and results

Each stage has two distinct effects:

- its **output** is connected to the next stage;
- its **result** is returned to the surrounding expression.

Only the final stage’s result can become the pipeline’s result. A non-final stage’s separate result is discarded. A value pipe passes its value to the next stage. A byte pipe passes only bytes.

In statement position, final byte output goes to standard output unless a redirect sends it elsewhere.

At a value boundary, such as the right-hand side of `let` or a captured block, ral captures final byte output. If the computation’s own result is `Unit`, the captured bytes become a `String`:

- decoding is strict UTF-8;
- one final `LF`, or one final `CRLF`, is removed;
- no other bytes are changed.

If the computation returns another value, that value wins; the captured copy of its bytes is not also returned. Invalid UTF-8 is an error. Use `from-bytes` when the output is binary.

```ral
let greeting = echo hello       # "hello"
let data = command | from-bytes # Bytes
```

Within a captured block, only the last command’s byte output becomes the block value. Earlier commands remain visible through the surrounding output stream:

```ral
let answer = !{ echo visible; echo captured }
# prints "visible"; answer is "captured"
```

Captures nest. Earlier output goes to the nearest enclosing visible stream. If a captured computation fails, bytes produced before the failure are flushed visibly rather than silently lost.

An in-memory capture keeps at most 16 MiB and marks truncated output. Redirect output to a file when it may be larger.

### 7.3. Explicit codecs

Codecs make every conversion between values and bytes visible in the source. A decoder reads its byte input and returns a value:

| Decoder | Result | Rules |
|---|---|---|
| `from-bytes` | `Bytes` | Preserves every byte. |
| `from-string` | `String` | Requires valid UTF-8. |
| `from-line` | `String` | Requires valid UTF-8 and removes one final `LF` or `CRLF`. |
| `from-lines` | `Stream String` | Splits lines and replaces invalid UTF-8 with the replacement character. It currently reads to end of input before returning the stream. |
| `from-json` | a ral value | Requires valid UTF-8 and JSON. JSON `null` becomes `Unit`. |
| `from-csv` | a list of records | Uses the header row as record keys. Fields are strings; duplicate headers are rejected. |

Decoders are nullary command forms. They read the preceding byte pipe, or standard input when there is no preceding stage.

An encoder takes a value in data-last position and writes bytes:

| Encoder | Accepted value | Output |
|---|---|---|
| `to-bytes` | `Bytes` or a list of integers from 0 through 255 | The represented bytes. |
| `to-string` | any value | Its ral textual form. |
| `to-line` | any value | Its textual form followed by a newline. |
| `to-lines` | a list of values | Textual forms separated by newlines, with no final newline. |
| `to-json` | a JSON-representable value | UTF-8 JSON. `Bytes` become an array of integers; variants become tagged objects. |
| `to-csv` | records accepted by the CSV codec | UTF-8 CSV. |

`to-json` rejects values with no faithful JSON representation, including blocks, lambdas, and handles. To decode a value already held in memory, encode it into a byte pipe first:

```ral
to-string $json_text | from-json
```

### 7.4. Redirects

Redirects attach input or output to one command or compound command:

| Form | Meaning |
|---|---|
| `> path` | Replace a file atomically when possible. |
| `>~ path` | Truncate the file and stream output into it. |
| `>> path` | Append output. |
| `2> path` | Stream standard error into a truncated file. |
| `2>&1` | Send standard error to standard output’s current destination. |
| `1>&2` | Send standard output to standard error’s current destination. |
| `< path` | Read standard input from a file. |
| `<< value` | Read standard input from a string value. |

The default descriptor is 0 for input redirects and 1 for output redirects. Redirects are applied from left to right, so descriptor duplication uses the destination established so far. Redirect targets are evaluated before their files are opened. Relative paths use the scoped logical working directory.

A redirect on a stage overrides the route the pipeline would otherwise use. In `cmd > file | next`, `cmd` writes to `file` and `next` sees end of input. If more than one redirect supplies standard input, the last one wins.

For a regular file, `>` writes a temporary file beside the destination, flushes it, then renames it over the destination. If the command fails, ral discards the temporary file and leaves the old destination unchanged. It preserves an existing file’s mode and follows a symbolic link to update its target rather than replacing the link.

This operation is failure-atomic, not a locking scheme: concurrent writers may still race. For non-regular destinations, `>` uses streaming behavior.

Use `>~` when readers should observe output as it is produced, or when writing a device, named pipe, or similar destination. `2>` is also streaming so diagnostics are not delayed until a command finishes.

### 7.5. Here strings

`<<` is a here string, not a heredoc delimiter:

```ral
from-json << '{"name":"Ada"}'
```

It feeds a string value to standard input. It does not add a newline. It removes exactly one leading `LF`, or one leading `CRLF`. This makes an indented multiline string convenient while preserving the rest of its contents, including any trailing newline.

Whitespace after `<<` is required. There is no `<<<` form, and `<<` can only redirect descriptor 0. A bare word after `<<` is rejected because it is usually a mistaken heredoc; use a quoted, interpolated, raw, or dereferenced string value.

Large here strings are written concurrently with the reader, so filling an operating-system pipe cannot deadlock command launch.

### 7.6. Process completion and the terminal

In a process-staged pipeline, ral launches all stages and waits for all of them. A failure in any stage fails the whole pipeline. If several stages fail, the first failing stage in launch order supplies the reported failure. A control escape such as `return` or `exit` takes precedence over an ordinary stage failure.

A producer often receives a broken-pipe signal when a downstream command intentionally stops reading, as in:

```ral
large-producer | head
```

ral treats that signal as success for a non-final stage. The same condition in the final stage remains a failure. Windows applies the equivalent rule to its pipe-closing status.

On Unix, an interactive process-staged pipeline receives the foreground terminal as one process group only when the session owns a terminal lease and final standard output is attached to that terminal. A captured pipeline normally writes to a buffer, so the parent keeps terminal ownership. A pure value pipeline never enters terminal job control.

Windows has no POSIX foreground-terminal handoff. Pipeline members share the console and are supervised as one job.

## 8. Control flow and failure

ral separates successful values from failure. A computation may successfully
return any value, including `false`, `unit`, an empty collection, or an empty
string. Failure is a separate control outcome carrying a nonzero status and a
message. This distinction is fundamental: `if` examines a `Bool`, while `?`
and `try` examine whether a computation succeeded.

Commands and control forms are checked before execution. Branches must agree
on their result type and on compatible byte-pipe and value-pipe behavior. A
branch that is not selected is not evaluated.

### 8.1. Sequencing and `return`

A newline or semicolon sequences computations from left to right:

```ral
prepare
let result = calculate
publish $result
```

Execution stops at the first failure. If every computation succeeds, the
sequence returns the value of its final computation; an empty sequence returns
`unit`. Bytes written by earlier computations remain observable even when a
later computation fails or when the sequence is evaluated under capture.

`return value` is the primitive successful computation. With no value,
`return` returns `unit`:

```ral
return 42
return
```

`return` is not a process exit and does not turn a false Boolean into failure.
A returned `Bool` does update `$STATUS` in the shell convention: `true` records
0 and `false` records 1. The computation itself succeeded in both cases.

### 8.2. Conditional choice

The conditional form is:

```ral
if condition { then-body }
elsif other-condition { other-body }
else { fallback-body }
```

Conditions are evaluated in order and must return `Bool`. The body belonging to
the first true condition runs. If none is true, the `else` body runs. The
selected body has a fresh lexical scope, so bindings made inside it do not leak
into the surrounding scope.

With `else`, all bodies must have a compatible result type. Their byte-pipe and
value-pipe behavior is joined into the type of the whole conditional. Without
`else`, the conditional is for effects: its result is always `unit`, whether
the body runs or not.

`false` chooses another branch; it does not cause `?` to continue and does not
invoke a `try` handler.

### 8.3. Variant elimination with `case`

`case` eliminates a variant using a tag-keyed record of handlers:

```ral
let result = case $reply [
  `ok:  { |value| return $value },
  `err: { |error| fail $error },
]
```

The scrutinee must be a variant. `case` selects the handler whose record key
matches the variant tag and calls it with the payload. A nullary variant passes
`unit`. Handler result types must agree.

When the handler table is a record literal, the typechecker verifies that it
covers every tag known in the scrutinee's variant type and reports a missing
handler before execution. A dynamically obtained handler table cannot always
be proved exhaustive; encountering an unhandled tag then fails at runtime and
names that tag. When the table contains other handlers, the diagnostic also
lists the tags it does handle.

### 8.4. Fallback chains

The `?` operator tries alternatives from left to right:

```ral
cached-value ? fetch-value ? fail [status: 1, message: 'no value available']
```

The first successful alternative supplies the chain's result. An ordinary
failure continues with the next alternative. If every alternative fails, the
chain propagates the final failure. Alternatives must have a compatible result
type, and each runs in a fresh lexical scope. Compatibility uses the joined
output behavior of the whole chain: when the chain's selected arm writes bytes,
a raw `unit` result is observed as the captured `String` at a binding boundary.

Control escapes are not alternatives. In particular, `exit`, a stopped job,
and internal tail-call control pass through a fallback chain rather than
selecting its next arm. Cancellation is checked between arms.

### 8.5. Raising failure

`fail` deliberately raises an ordinary, recoverable failure. It accepts one
error record:

```ral
fail [status: 64, message: 'bad command line']
```

A record must contain a nonzero integer `status` field. An optional `message`
field may be `String` or `Bytes`; an absent message uses `explicit failure`.
Bytes are decoded lossily for the diagnostic. The checker enforces this shape
when it is statically known. If a dynamically obtained record contains a
message of another kind, the runtime uses the default message. Other fields are
ignored when `fail` raises the error.

Status 0 is rejected: use `return` for success. A status must fit the runtime's
signed exit-code range. Passing the error record received by `try` to `fail`
re-raises its essential status and message. Integers and bare message strings
are rejected so failure remains structured at its point of construction.

An external command that exits unsuccessfully and a runtime operation that
cannot complete also produce ordinary failures. They participate in `?` and
`try` in the same way as `fail`.

### 8.6. Recovery with `try`

`try` evaluates a body and, only if that body fails, calls a one-argument
handler:

```ral
let value = try {
  read-config
} { |error|
  log $error[message]
  return default-config
}
```

On success, `try` returns the body's value. On recoverable failure, the handler
receives this record:

```ral
[
  cmd: String,
  status: Int,
  message: String,
  line: Int,
  col: Int,
]
```

`cmd` is the last failing execution call recorded for the body, including a
builtin call, or `<runtime>` when no failing call was recorded. `line` and
`col` identify the failure's source position. The body and handler must produce
compatible results.

`try` is control flow, not output capture. Bytes written to standard output or
standard error continue through the surrounding byte pipes. The error record's
`message` is the structured failure message; it is not captured stderr. Use
`audit` when command bytes and the full execution tree are required.

A handler that succeeds recovers the failure and leaves status 0. `exit`, a
stopped job, and internal tail-call control bypass the handler. Cancellation
may be observed as a recoverable error inside the body, but cancellation is
sticky: recovering it cannot make the enclosing run complete successfully.

### 8.7. Cleanup with `guard`

`guard` pairs a body with a cleanup block:

```ral
guard {
  use-resource
} {
  release-resource
}
```

The cleanup runs after the body whether the body succeeds or fails. Under
normal completion, `guard` returns the body's value. If the body fails, its
failure remains the result after cleanup.

An ordinary cleanup failure is reported as `guard: cleanup failed: ...` but
does not replace the body's outcome. A cleanup control escape, such as `exit`
or a stopped job, does take priority and propagates; discarding a stopped-job
escape would lose the process group needed to resume or reap it.

Both blocks contribute to the guard's byte-pipe and value-pipe type. Output
from cleanup is real output and is not silently captured. Bindings created in
either block remain local to that block.

### 8.8. Exiting the session

`exit` (also spelled `quit`) requests that the host end the current ral
session:

```ral
exit
exit 7
```

The default status is 0. An explicit status must be an integer in the signed
exit-code range. `exit` is a control escape, not an ordinary failure: `?` and
`try` do not catch it. A command-line host translates it to the platform's
process exit status; an embedding host receives the structured exit request.

### 8.9. Diagnostics

Parse, type, and runtime errors identify their source and underline the
relevant span. Type errors are reported before any part of the checked program
runs. Runtime failures retain the innermost available source position while
they unwind; code loaded from another file is diagnosed against that file, not
against an unrelated caller.

When a failure has actionable context, ral may add a hint beneath the primary
message. A failing external command may also contribute its own exit-status
hint. These diagnostics are presentation: recovery uses the structured status,
message, command, and source position described above, never by parsing the
rendered text.

### 8.10. Cancellation and signals

Cancellation is cooperative and scoped. The evaluator polls at stable
boundaries, including between sequence elements and fallback arms, during
collection loops and tail-call steps, and while launching or waiting for
pipelines and external commands. A cancelled external process is terminated
and reaped before control returns.

Cancellation causes form an escalation order and never downgrade:

| Cause | Message | Status |
|---|---|---:|
| foreground interrupt | `interrupted` | 130 |
| explicit cancellation | `cancelled` | 130 |
| deadline | `timed out` | 130 |
| session termination | `terminated` | 143 |
| root abort | `aborted` | 130 |

A foreground interrupt affects the work that was running when the interrupt
arrived, including nested runs, but not a detached worker and not a later
prompt. Explicit cancellation targets its handle. Deadlines affect their run
scope. Session termination and root abort reach the durable session root and
therefore its detached workers as well as foreground work.

On Unix, Ctrl-C is a foreground interrupt. At an idle interactive prompt it
cancels the edited line instead. Ctrl-\\ requests a root abort; `SIGTERM` and
`SIGHUP` request session termination. The batch host escalates repeated
termination signals and may force an immediate process exit when cooperative
cleanup no longer completes. Interactive Ctrl-C itself does not use that
force-exit ladder.

Other hosts translate their native gestures into the same structured causes.
For example, an active exarch request treats Ctrl-C or Escape as a foreground
interrupt, while an idle key may instead close its interface. Windows uses its
console and process-group facilities rather than Unix job-control signals, but
preserves the observable cancellation messages and statuses where applicable.

## 9. Scoped execution and handlers

ral uses two kinds of scope for two different purposes:

- Values are lexically scoped. A function or block sees the bindings that existed where it was written.
- Execution context is dynamically scoped. The effective directory, environment overlay, handlers, and capability restrictions come from the call site and remain active through everything the body calls.

```ral
let place = 'definition'
let show = { echo $place }

within [env: [MODE: 'test']] {
    let place = 'call site'
    show                 # prints "definition"
    echo $ENV[MODE]      # prints "test"
}
```

`let` bindings are immutable. A binding in a deeper lexical scope shadows an outer binding without changing it. Closures retain their defining lexical environment, while commands run by those closures use the caller’s dynamic context.

### 9.1. `within`

`within` installs a dynamic execution context for one block:

```ral
within [
    dir: 'project',
    env: [MODE: 'test', RETRIES: 3],
    handlers: [deploy: { |args| return [mock: true, args: $args] }],
    handler: { |name args|
        fail [status: 127, message: "unexpected command: $name"]
    },
] {
    deploy staging
}
```

The options value must be a map. It accepts exactly four keys:

- `dir` — the effective working directory;
- `env` — environment-variable overrides;
- `handlers` — named command handlers;
- `handler` — one catch-all command handler.

Unknown keys are errors. All options are evaluated and validated before the body begins. The environment scope is installed outermost, then the directory scope, then the handler frame. This makes the environment visible while commands resolve beneath the directory and handler scopes.

Validation uses the incoming dynamic context. Options in the same map do not
take effect one by one: for example, `dir` is resolved before the new `env`
overlay is installed, so a `HOME` override beside it does not change how that
`dir` value is resolved.

The frame remains active for the body’s whole dynamic extent, including functions defined elsewhere, nested blocks, command handlers, spawned work, and every landing of a tail-recursive call. Leaving normally, failing, or escaping restores the surrounding dynamic context.

### 9.2. Directory scope

`dir` is converted to a path string, resolved against the current effective directory, checked for filesystem-read authority, and required to name an existing directory.

```ral
within [dir: 'src'] {
    cat 'main.ral'
}
```

The scoped directory is used by ral path operations, relative executable lookup, redirects, module lookup, and external children. ral does not change the process-wide working directory; each child is launched in the effective logical directory. This avoids races between concurrent workers.

`dir` is an override, not a `cd`. A `cd` performed beneath it may update the block’s underlying cwd state, but the `dir` override remains the effective directory until `within` exits. Because a `within` body is a block, that underlying cwd mutation is discarded when the block closes.

An empty path, a missing path, a non-directory, or a path denied by the active filesystem grant is an error before the body runs.

### 9.3. Environment scope

`env` must be a map. Each value must be a string, integer, float, or Boolean; non-string scalars are converted to strings.

```ral
within [env: [PATH: 'tools:/usr/bin', DEBUG: true]] {
    build
}
```

An inner overlay shadows the same key in an outer overlay. Other keys remain inherited. The effective overlay is used by `$ENV`, `$USER`, home and XDG resolution, `PATH` lookup, `RAL_PATH`, capability-path resolution, and external child environments.

`PWD` and `OLDPWD` cannot be set through `within env`. They are derived from ral’s logical cwd; use `cd` or `within dir` instead. Lists, maps, blocks, handles, and other non-scalar environment values are rejected.

The previous environment overlay is restored exactly when the scope ends. A spawned worker receives a snapshot of the overlay in force at birth; later changes in either shell do not cross back.

### 9.4. Named and catch-all handlers

A named handler reinterprets one command name:

```ral
within [handlers: [
    git: { |args|
        echo "git called with: $args"
        return unit
    },
]] {
    git status
}
```

Every value in `handlers` must be a unary lambda `{ |args| ... }`. It receives one `List` containing the command’s evaluated ral arguments.

The singular `handler` field is a catch-all and must be a binary lambda `{ |name args| ... }`. It receives the command name as a `String` and its arguments as a `List`.

A bare block, non-lambda value, or lambda with the wrong number of parameters is rejected when the frame is installed. A named handler must also preserve the command head’s byte-pipe/value-pipe modes when that head already has a known mode. A handler may change the returned value, but it cannot silently turn a byte-producing command into a value-only command or otherwise invalidate surrounding pipelines; use an explicit codec when conversion is intended.

Handlers are command operations, not first-class names. They are invoked in command position and cannot be fetched with `$name`.

### 9.5. Command resolution and handler precedence

A bare command head is resolved in this order:

1. the lexical value environment, including fixed-arity native commands;
2. the innermost named handler for that spelling, searching all active run frames;
3. a base command supplied by ral or the embedding host;
4. the innermost catch-all handler;
5. an external executable found through `PATH`.

Named handlers are searched in a complete innermost-first pass before catch-alls. Therefore any named handler, even in an outer frame, outranks every catch-all. Base commands likewise outrank catch-alls. A named handler can intercept a base command because named run frames are checked first.

A lexical binding still wins over every handler. Installing a handler under an already-bound name is allowed, but a normal bare call will not reach it. The explicit head `^name` skips the lexical environment while still consulting named handlers, base commands, and catch-alls. A path head such as `./tool` or `/usr/bin/tool` skips handlers entirely and executes that path.

Aliases use the same named-handler stack but persist beyond a `within` block until removed or replaced. Scoped handler frames themselves are removed when their `within` ends.

### 9.6. Deep, self-masking handlers

Handlers are deep: a frame remains installed after one interception, so every matching command in the rest of the body reaches it.

They are also self-masking. While a matched handler body runs, ral temporarily removes the entire matched frame. A same-name call in that body therefore reaches the next outer named handler, a base command, a catch-all in another frame, or the external command. It never immediately calls itself.

```ral
within [handlers: [git: { |args|
    echo 'before'
    git ...$args          # forwards past this frame
    echo 'after'
}]] {
    git status
}
```

Masking the frame rather than only one entry also means its sibling named handlers and its catch-all are unavailable while that handler body runs. Older and newly nested frames remain visible. ral restores the removed frame in its original order after success, failure, or escape.

There is no first-class `resume`. Returning from the handler supplies the intercepted command’s result and execution continues exactly once. A handler may instead fail or forward by calling the command again under self-masking.

Redirects apply to handler dispatch just as they do to other command calls. Bytes emitted by the handler or a command it forwards reach the active redirects and byte pipes.

### 9.7. State flow and containment

The body of `within` is a block. It runs with a fresh lexical frame and discards its mobile program state when it closes. Bindings, aliases, module registrations, handler changes, cwd changes, and similar mutations made inside do not escape. The final command status does escape and becomes the caller’s `$STATUS`.

Boundary rules are deliberately specific:

- A forced block enters with the caller’s current status and returns only its final status; cwd and bindings remain private.
- An ordinary lambda call enters with status 0. Its final status and logical cwd flow back to its caller, while its lexical locals remain private. Inside a surrounding `within dir`, that dynamic directory override still wins, and the enclosing block ultimately contains the cwd change.
- A value pipeline runs sequentially in the evaluator, so it follows the ordinary function and block rules.
- Each stage of a process-staged pipeline runs in a child process with a snapshot of the active lexical and dynamic context. Stage-local bindings, cwd changes, environment changes, aliases, and handlers do not return. If every stage succeeds, the final stage determines the pipeline status. Otherwise the first stage failure observed in launch order is propagated; a control escape takes priority over an ordinary failure. Returned values and audit data cross only through their defined channels.
- A spawned worker receives the closure’s lexical capture and a snapshot of the dynamic context, including directory, environment, handlers, arguments, and grants. It starts with status 0. Its cwd, bindings, and later dynamic changes remain private, while its result returns through the handle.
- An external process receives the effective cwd and environment at launch. It cannot mutate ral’s shell state.

These containment rules are the same on every supported host. Host differences affect executable lookup, path syntax, and the implementation of process-staged pipelines and process control, not lexical scope, handler precedence, or dynamic restoration.

## 10. Programs, modules, and session state

A ral program may come from a file, command-line text, standard input, or an interactive session:

```text
ral build.ral release     # run a file
ral -c 'echo hello'       # run command-line text
ral -s                     # read a program from standard input
ral                        # start the REPL when stdin is a terminal
```

A script is parsed and typechecked before it starts. Runtime-loaded files are parsed and typechecked when `source` or `use` reaches them.

### 10.1. Program arguments and identity

Arguments after a script path or `-c` program are available through `$ARGS` as a list of strings:

```text
ral deploy.ral staging eu-west
```

```ral
let [environment, region] = $ARGS
```

`$ARGS` does not include the ral executable, the script path, or the `-c` source text. It is empty in the REPL and for a program read from standard input.

`$SCRIPT` identifies the file containing the reference:

```ral
echo $SCRIPT
```

It is lexical: a reference inside a loaded module names that module, not the file that loaded it. ral fixes the value while compiling the file, so a later call from another file cannot change it.

Only real script files have this identity. Using `$SCRIPT` in the REPL, `-c`, standard-input programs, preloaded sources, or other synthetic source is a compile-time error.

Without an explicit script or `-c`, ral starts the REPL when standard input is a terminal and reads a batch program otherwise. `-i` forces the REPL; `-s` forces a standard-input program and takes precedence over `-i`. An explicit script path takes precedence over both.

A successful batch program exits with its final recorded status. `exit n` exits with `n`. Parse errors, type errors, runtime errors, and failed external commands produce a nonzero status and an explanatory diagnostic. Process exit statuses are reduced to the range 0 through 255.

### 10.2. Ambient program values

The following names describe the live shell:

| Name | Value |
|---|---|
| `$ENV` | Environment variables as a map of `String` to `String`. |
| `$CWD` | The logical working directory, abbreviated with `~` when it lies under the current home directory. |
| `$STATUS` | The most recently recorded command status as an `Int`. |
| `$USER` | `USER`, or `USERNAME` where appropriate, from the effective environment. |
| `$NPROC` | Available processor parallelism as an `Int`, never less than 1. |
| `$ARGS` | The current program’s argument strings. |

These values are computed when read. They are not mutable shell variables.

`$ENV` combines the host process environment with any active ral overrides. An inner override wins:

```ral
within [env: [MODE: 'test', USER: 'builder']] {
    echo $ENV[MODE]
    echo $USER
}
```

Environment overrides are dynamically scoped. They affect `$ENV`, `$USER`, home and command lookup, `RAL_PATH`, and child processes, then disappear when the `within` body ends. There is no general `setenv` operation in the language.

`PWD` and `OLDPWD` are deliberately absent from `$ENV`. ral owns its working directory separately so concurrent computations never race over the process-wide current directory. Child commands receive `PWD`, `OLDPWD`, and their actual process working directory from this logical state.

`cd path` changes the session’s logical working directory. Relative paths, file operations, module loads without a containing file, command lookup, and child processes all use it. A top-level `cd` persists into later runs. A `within [dir: path]` override lasts only for its body.

A function call carries its `cd` result back to its caller. A forced block is a local computation boundary: its bindings and working-directory changes are discarded when it finishes, while its final status is retained.

### 10.3. Loading a file with `source`

`source path` evaluates another ral file in the caller’s current scope:

```ral
source 'shared/settings.ral'
echo $project_name
```

Bindings created by the file, including names beginning with `_`, become caller bindings. `source` returns the file’s final value.

A relative path inside a script or module is resolved from the directory containing that file. A relative path at the REPL or in source with no file identity is resolved from `$CWD`.

`source` is not transactional. If a loaded file creates bindings and later fails, the earlier changes have already happened. The failure keeps its original status and is reported with a `source:` prefix. Its diagnostic points into the loaded file.

### 10.4. Importing a module with `use`

`use path` evaluates a file in a fresh top-level scope and returns its public bindings as a map:

```ral
let math = use 'lib/math.ral'
echo $math[mean] [1, 2, 3]
```

Bindings do not leak into the caller. Names beginning with `_` are private and are omitted from the returned map:

```ral
# lib/math.ral
let _sum = { |xs| fold add 0 $xs }
let mean = { |xs| $[_sum $xs / length $xs] }
```

The module’s final expression is evaluated, but `use` returns the binding map rather than that expression’s value.

`use` first resolves a path relative to the containing file, or relative to `$CWD` when there is no containing file. If that path does not resolve, ral searches the directories in the effective `RAL_PATH`, in order. The effective value is read when `use` runs, so a dynamically scoped `within [env: [RAL_PATH: ...]]` override controls only loads in that body. `RAL_PATH` uses the platform’s normal path-list separator: `:` on Unix and `;` on Windows. Each search candidate must be a regular file; a directory with the requested name does not stop the search of later entries.

`source` does not search `RAL_PATH`. It always treats its argument as a file path.

### 10.5. Module freshness, cycles, and errors

ral does not cache loaded modules. Every `source` and every `use` reads, compiles, and evaluates the file again:

```ral
let first = use 'config.ral'
# after config.ral is edited:
let second = use 'config.ral' # sees the new file
```

This also means module side effects run on every load. Bind a module map once when repeated evaluation is unwanted.

During a nested load, relative imports use the innermost file’s directory. ral keeps the active load chain and rejects a file that appears again in that chain:

```text
use: circular dependency: a.ral -> b.ral -> a.ral
```

The active load depth is limited to 100 files. Exceeding it is an error rather than an uncontrolled recursion.

Both loaders check filesystem read authority before reading. A denied load fails before the module body runs. Missing files, permission failures, parse errors, type errors, cycles, depth overflow, and failures raised by the module retain their useful status and are prefixed with `source:` or `use:`.

### 10.6. Persistent sessions

A batch invocation owns one fresh shell and ends with the process. A REPL or embedding host may submit many top-level runs to one session.

Successful top-level changes persist between those runs, including:

- `let` bindings and their inferred types;
- aliases and loaded definitions;
- the logical working directory;
- the last status.

Changes completed before a later command fails also persist:

```ral
let ready = true
failing-command
```

A later REPL run can still read `$ready`; statements after the failure never ran.

Nested blocks remain local unless their contract explicitly returns an observation. This separation lets an interactive session accumulate deliberate top-level state without making every temporary block mutation permanent.

Interactive sessions load their rc file unless `--norc` (or its
`--noprofile` alias) is given. Login sessions additionally load the system and
user login profiles. These are ral source with host-level result contracts:
each login profile must return `Unit`, while the rc file must return a
configuration `Map`. Their evaluation is not transactional, so bindings and
effects completed before a runtime failure, `exit`, or result-contract check
remain part of the session. Applied RC bindings, aliases, environment
configuration, hooks, and startup behavior likewise belong to that session.

## 11. Concurrency and process lifetime

ral distinguishes concurrent work by who still owns it:

- an ordinary command belongs to the current run;
- `spawn`, trailing `&`, and `watch` create a worker owned by the session;
- `service` creates a session-owned worker exempt from ordinary worker leases;
- `detach` gives an operating-system process away, so the session no longer
  owns or controls it.

This distinction is visible in the result type. Session-owned work returns a
`Handle`. Surrendered work returns only a receipt.

### 11.1. Spawning work

`spawn` starts a block concurrently and returns immediately:

```ral
let build = spawn {
    cargo build
    return 'built'
}

echo 'the build is running'
let result = await $build
echo $result[value]
```

Trailing `&` is syntax for the same operation:

```ral
let build = cargo build &
let filtered = cat large.log | grep error &
```

More precisely, `command &` lowers to `spawn { command }`. It therefore
returns the same `Handle` and follows the same output, cancellation, registry,
and lifetime rules. It does not create a REPL process-group job.

The worker receives the lexical environment captured by its block. Values are
immutable, so concurrent workers do not share mutable lexical bindings. Each
worker has its own shell state: changes such as `cd`, environment overrides,
or status changes in the worker do not mutate the spawning thread's mobile
state. A nested `spawn` remains owned by the same session.

A worker is not part of the foreground run. Interrupting or timing out the run
that created it does not cancel the worker. The worker's standard input is
empty rather than inherited from the terminal, so detached work cannot steal
interactive input.

Byte pipes and value pipes retain their ordinary meaning inside a worker. A
byte pipe still connects byte-producing and byte-consuming stages; a value
pipe still folds typed values. Concurrency changes lifetime, not pipeline
typing.

### 11.2. Handles and eliminators

A handle is a process-local capability for one worker. The core eliminators
are `await`, `poll`, `race`, and `cancel`.

#### `await`

`await handle` blocks until the worker settles. On success it returns:

```text
[
    value:  A,
    stdout: Bytes,
    stderr: Bytes,
]
```

`A` is the return type of the spawned block. The type of the handle and the
type of `value` are linked, so a consumer cannot assume a different result
type.

If the worker fails, exits, or panics, `await` raises that failure rather than
returning the record. Use `try` when failure is expected:

```ral
try {
    let result = await $build
    use $result[value]
} { |error|
    echo "build failed with status $error[status]"
}
```

The settled result is cached. Repeating `await` on a completed handle returns
the same value and bytes. A foreground interruption of `await` interrupts the
wait, not the durable worker; the handle may be observed later.

#### `poll`

`poll handle` never waits. It returns one of two variants:

```text
`pending [stdout: Bytes, stderr: Bytes]

`settled [
    stdout: Bytes,
    stderr: Bytes,
    outcome: <`ok A | `err ErrorRecord>,
]
```

The pending bytes are a cumulative, non-destructive snapshot. Polling does not
consume them, so a later poll or `await` still sees the complete buffered
output. Repeated pending polls may therefore return progressively longer byte
sequences.

The settled arm reports worker failure as `outcome: `err ...`; it does not
raise that failure. Thus `poll` itself succeeds and leaves status zero when it
successfully samples either arm. The prelude function `is-done handle` returns
`false` for `pending` and `true` for either settled outcome.

#### `race`

`race handles` waits until it observes one handle settle, returns the same
record as `await`, and cancels every remaining live handle:

```ral
let a = spawn { fetch 'https://a.example/data' }
let b = spawn { fetch 'https://b.example/data' }
let first = race [$a, $b]
```

If several handles are already settled when inspected, list order breaks the
tie. A failed winner is re-raised after the losers have been cancelled. An
empty list, or a list with no live or settled handle left to observe, is an
error.

#### `cancel`

`cancel handle` requests explicit cancellation and removes the worker from the
owning registry. Cancellation is cooperative for ral computation and is
propagated to external child process groups by the process runtime. A worker
may take a short bounded interval to tear its process tree down.

Cancellation has no result. `await` and `poll` on a cancelled handle fail with
`handle is cancelled`; the diagnostic suggests using `try` around `await`.
Cancelling a handle that has already completed does not destroy its cached
outcome.

### 11.3. Buffered output and `par`

Each ordinary worker has independent stdout and stderr byte buffers. Its bytes
do not appear on the spawning run's output while it executes. `await` and
settled `poll` return them as `Bytes`; pending `poll` exposes the bytes written
so far.

Each buffer is capped at 16 MiB. At the cap ral appends one truncation marker,
continues draining the writer, and discards later bytes. Draining continues so
a chatty child cannot deadlock on a full kernel pipe, while memory remains
bounded. Redirect high-volume output inside the spawned block when the full
stream matters:

```ral
let server = spawn {
    python -m http.server > server.log 2> server.err
}
```

Explicit redirects bypass the corresponding handle buffer. A watched worker
also has empty completion buffers because its bytes were streamed live.

`par function items jobs` is the standard parallel map:

```ral
let converted = par { |path| convert-one $path } $paths 4
```

At most `jobs` workers run at once. A non-positive limit permits one worker per
item. Results preserve input order even when workers finish out of order.
`par` unwraps each await record and returns only the list of `value` fields;
use explicit `spawn` and `await` when stdout or stderr is needed. If an item
fails, `par` cancels every remaining sibling before re-raising the original
failure.

### 11.4. Registry, leases, and shutdown

Every `spawn`, trailing `&`, `watch`, and `service` birth is registered at once
in the owning shell. A registry entry has a stable `wN` identity, description,
start time, lifetime class, state, and the live handle. Nested workers share
the owning shell's registry.

An entry leaves when:

- `await`, settled `poll`, or `race` observes its result;
- `cancel` removes it;
- a host worker lease reaps it; or
- a host retention policy reaps an unclaimed settled result.

A pending poll leaves the entry in place. Listing workers is also a pure
observation and renews nothing.

Hosts may give ordinary workers an idle-observation lease with an absolute
backstop. `poll`, `await`, and `race` renew the idle clock whenever they name a
handle; continuous waiting renews it on every sweep. Observation can postpone
idle reaping but never the absolute backstop. A lease reap cancels the worker
and records a notice explaining whether the idle bound or backstop fired.

Hosts may also cap live workers. Admission reserves a seat before the thread
is created, so concurrent births cannot overshoot the cap. Only running or
being-born workers occupy seats; settled results retained for observation do
not. A refused birth reports the cap and suggests awaiting or cancelling a
worker.

These bounds are host policy, not core constants. The `ral` interactive and
batch hosts arm no worker lease, retention bound, or worker cap. The exarch
agent host currently grants ordinary workers a one-hour idle bound, a 24-hour
absolute backstop, a cap of 64 live workers, and retention of 256 ral calls for
unclaimed settled entries.

All worker classes remain owned by the session and are cancelled when that
session is destroyed. Teardown then waits briefly and boundedly for worker
threads to act on cancellation and tear down their children. The interactive
shell first warns once with the identities of workers still running. Workers
do not survive process exit; use `detach` when survival is the requirement.

### 11.5. Host-installed worker forms

The core language provides `spawn`, `await`, `poll`, `race`, and `cancel`.
`watch`, `service`, and `detach` are optional host builtins. If a host does not
install one, its name is absent from that shell's builtin table and is checked
and resolved like any other unknown command.

#### Live output with `watch`

`watch label block` creates an ordinary session worker whose output is streamed
instead of buffered:

```ral
let build = watch 'build' { cargo build }
await $build
```

Each stdout line is prefixed `[build] ` and each stderr line `[build:err] `.
Sibling watchers serialize complete framed lines, though their lines may
interleave. A partial final line is flushed when the worker settles.

The returned value is an ordinary handle, so `await`, `poll`, `race`, and
`cancel` apply. Its stdout and stderr buffers remain empty because those bytes
have already been sent to the durable output sink. Programs run through the
`ral` executable, in interactive and batch modes, install `watch`. A host whose
output capture ends with each run, including exarch, omits it because a worker
cannot safely retain that run's sink.

#### Durable session work with `service`

`service description block` creates a buffered worker with no idle lease and
no absolute backstop:

```ral
let server = service 'local documentation server' {
    python -m http.server 8000
}
```

The description must be a non-empty, single-line `String`. It is the service's
stable explanation in host listings. A service still occupies a live-worker
seat and still ends on explicit `cancel`, session destruction, or process
exit. “Durable” means exempt from neglect-based reaping, not independent of
the host process.

Agent hosts with ordinary worker leases may install `service`; exarch does.
The `ral` executable omits it because its ordinary workers already have no
idle lease or backstop. A host may provide an id-based way to recover service
handles; exarch exposes its live services in a host-owned register and provides
`service-handle id`.

#### Work that survives with `detach`

`detach description command arguments...` starts an operating-system process
that the session deliberately stops owning:

```ral
let receipt = detach 'local documentation server' python -m http.server 8000
echo $receipt[pid]
```

The description must be a non-empty, single-line `String`. The result is:

```text
[pid: Int, desc: String]
```

This record is a receipt, not a handle. `await`, `poll`, `race`, and `cancel`
do not apply. The pid is the process's number at birth, not an enduring
capability; operating systems may later recycle it.

The surviving process has standard input, standard output, and standard error
connected to the null device. It has no buffered result and its exit status is
unrecoverable. A successful return says that the program was launched; it
does not prove that it remained alive or initialized successfully. A detached
program must arrange its own logs, and callers must probe the resource it is
supposed to provide.

On Unix the implementation double-forks, reparents the grandchild, and creates
a new session. It preserves the logical working directory, environment, and
the sandbox projection active at birth. Later grants cannot widen it. An
active grant may refuse the operation with `detach: false` before any process
is born.

`detach` is Unix-only and is installed only by hosts that also provide a
successful-birth budget. The budget is shared by nested workers, counts only
committed births, and is not replenished because the session cannot observe a
detached process's death. Exarch currently permits 16 births per session.
Neither interactive nor batch `ral` installs `detach`; Windows has no such
builtin.

### 11.6. Interactive job control

REPL job control is separate from handles. On Unix, pressing Ctrl-Z while a
foreground external command or byte-pipe process pipeline runs stops its whole
process group. ral returns the terminal to the shell, records the group, and
prints a notice such as:

```text
[1] stopped	vim notes.txt (SIGTSTP)
```

When any stage of a byte-pipe process pipeline stops, ral stops the remaining
stages so the pipeline is parked as one job. A pure value pipe is evaluated as
an in-process fold and has no process group to stop.

The interactive builtins are:

| Command | Contract |
|---|---|
| `jobs` | List process-group jobs and session workers without changing either population. |
| `fg id` | Resume a process-group job in the foreground and wait until it exits or stops again. |
| `bg id` | Resume a stopped process-group job in the background. |
| `disown id` | Remove a process-group job without signalling it; the session no longer owns it. |

The id is optional for `fg`, `bg`, and `disown`; omission selects the most
recent process-group job. Job designators are decimal integers. There is no
`%1`, `%+`, or `%-` syntax.

`jobs` folds two kinds of residents into one listing. Process groups use `[N]`
and report `running` or `stopped`, their pgid, and original command. Workers
created by `spawn`, `watch`, or trailing `&` use `[wN]` and report `running
(worker)` or `done (worker)`. Listing renews no worker lease and removes
nothing.

`fg`, `bg`, and `disown` accept only process-group job ids; they do not accept
`wN`. The corresponding handle operations are `await` for foreground waiting
and `cancel` for termination. A worker is already detached from the foreground
and has no `bg` operation.

The REPL does not splice asynchronous “Done” messages into the prompt and does
not refuse exit because jobs exist. State is observed explicitly with `jobs`.
On exit, every undisowned process-group job receives a polite termination
request and five seconds to exit; survivors are killed. On Unix a stopped
group is continued after the termination signal so it can perform cleanup.
Disowned process groups are outside this sweep.

Workers follow the session teardown rule instead: the REPL names running
workers once, cancels them, and waits briefly and boundedly for teardown.
There is no `disown` for a handle. To create work intended to outlive the
process, use a host that provides `detach` and accept its receipt-only contract.

Job parking requires an interactive terminal. In scripts, captured-output
contexts, and other non-foreground runs, a stopped external pipeline is killed
and reported as an error because there is no job table or terminal to return
to. Windows has process-group bookkeeping for teardown but no SIGTSTP analogue,
so a live Windows REPL cannot acquire stopped jobs through Ctrl-Z.

### 11.7. Availability summary

| Facility | Core / portable | `ral` interactive | `ral` batch | exarch agent host |
|---|---:|---:|---:|---:|
| `spawn`, trailing `&`, `await`, `poll`, `race`, `cancel` | yes | yes | yes | yes |
| `par`, `is-done` from the standard prelude | when that prelude is installed | yes | yes | yes |
| `watch` | optional | yes | yes | no |
| `service` | optional | no | no | yes |
| `detach` | optional, Unix only | no | no | Unix sessions that enable it |
| `jobs`, `fg`, `bg`, `disown` | no | yes | no | no |

Handles are local runtime values. They may be stored, passed to functions, and
returned through locally evaluated blocks, but they cannot cross a serialized
child-evaluation or remote-host boundary. Await the handle before crossing such
a boundary.

## 12. Capabilities and sandboxing

`grant` temporarily reduces the authority available to its body and to work started by that body.

```ral
grant [
    exec: [
        git: ['status', 'diff'],
        sh: 'deny',
    ],
    fs: [
        read:  ['cwd:', '/usr/share'],
        write: ['cwd:build'],
        deny:  ['cwd:.git'],
    ],
    net: false,
    detach: false,
] {
    git status
}
```

Each dimension is independent. Omitting `net`, for example, says nothing about network access; it inherits the surrounding authority. An empty grant changes nothing:

```ral
grant [] {
    # ambient authority
}
```

A grant can never restore authority withheld by an outer grant or a session profile. Nested grants intersect:

```ral
grant [exec: [git: ['status', 'diff']]] {
    grant [exec: [git: ['status']]] {
        git status   # allowed
        git diff     # denied
    }
}
```

A denial at any layer remains a denial.

### 12.1. Capability fields

A capability map accepts exactly these keys:

- `exec`
- `fs`
- `net`
- `detach`
- `audit`
- `editor`
- `shell`

Unknown keys are errors.

Omitting a whole field inherits that dimension. Once a structured field such as `fs`, `editor`, or `shell` is present, omitted members inside it take their restrictive default.

Thus:

```ral
grant [fs: [read: ['cwd:']]] { … }
```

allows reads beneath the frozen working directory, denies all writes, and leaves every non-filesystem dimension unchanged.

Similarly:

```ral
grant [editor: [read: true]] { … }
```

allows editor reads, denies editor writes and TUI access, and leaves non-editor dimensions unchanged.

### 12.2. `exec`

`exec` is a map from command names or paths to a policy:

```ral
exec: [
    git: ['status', 'diff'],
    rg: 'allow',
    bash: 'deny',
    '/opt/tools/': 'allow',
]
```

A literal command accepts:

- `'allow'` — any arguments;
- `'deny'` — no invocation;
- a non-empty list of strings — only invocations whose first argument is in that list.

The subcommand check examines only the first argument. A command with no first argument is denied by a subcommand policy.

A key ending in `/` names every executable beneath that directory. Directory keys accept only `'allow'` or `'deny'`. The most specific matching directory wins; an equal allow and deny resolves to deny. A literal denial also vetoes the command by basename, so `bash: 'deny'` cannot be avoided by spelling `/bin/bash`.

Once an `exec` map is present, commands not admitted by that map are denied. Nested `exec` maps intersect, subcommand lists intersect, and every denial remains effective.

Two exec-only shorthands expand when the grant is decoded:

- `'path:'` — every absolute directory in the current `$PATH`;
- `'system:'` — ral’s platform-defined system tool directories.

Both take only `'allow'` or `'deny'`. `path:` is an error if `$PATH` contains no absolute directories.

Bare command names still resolve through the effective `PATH` when invoked. For tighter control, use an absolute executable path or a frozen directory rule.

### 12.3. `fs`

`fs` has three path lists:

```ral
fs: [
    read:  ['cwd:', 'xdg:data'],
    write: ['cwd:build', 'tempdir:'],
    deny:  ['cwd:.secrets'],
]
```

- `read` permits reads under the listed prefixes.
- `write` permits writes under the listed prefixes.
- `deny` forbids both reads and writes under the listed prefixes.

Read and write authority are separate: write permission does not imply read permission. A present `fs` map with no `read` or no `write` entry grants nothing for that operation. Deny regions are carve-outs and accumulate across nested grants.

ral resolves the accessed path, including symlinks, before testing prefix membership. Policy prefixes also carry a symlink-resolved identity captured when the policy is decoded. Nested policies are intersected using that frozen identity; point-of-use checks resolve the live path again. `/dev/null` is always permitted as a discard device.

ral checks filesystem operations that it performs itself. A spawned program’s own filesystem access is instead confined by the host sandbox where supported.

### 12.4. Path sigils and freezing

Capability paths must be absolute after expansion. These sigils make portable absolute prefixes:

- `~` or `~/subpath` — the effective home directory;
- `xdg:config`, `xdg:data`, `xdg:cache`, `xdg:state`, `xdg:runtime`, and their subpaths;
- `cwd:` — the working directory at decode time;
- `tempdir:` — the platform temporary directory at decode time;
- `gitdir:` — the current repository’s real Git directory, or `cwd:` outside a repository.

Sigils, `.` and `..`, environment-derived bases, and symlink identities are frozen when the inline grant or profile is decoded. Later `cd`, `$HOME`, `$TMPDIR`, XDG, or repository changes do not retarget the grant.

Ordinary relative paths are rejected. Use `cwd:relative/path` when that is intended.

An XDG sigil must resolve beneath the effective home directory. ral rejects an unknown XDG name, an unset home needed by `~` or `xdg:`, or an XDG environment override that would escape the home directory. A path rooted for another operating-system family is ignored as a dead, non-matching grant so one profile can contain host-specific entries.

### 12.5. `net`

`net` is a Boolean:

```ral
grant [net: false] { … }
```

`false` disables network access for spawned children through the OS sandbox. `true` leaves network authority available unless an outer layer already disabled it. ral has no in-process network primitive, so there is no separate ral-level network check.

If the host cannot enforce `net: false`, ral refuses the confined launch rather than silently running it online.

### 12.6. `detach`

`detach` is a Boolean controlling whether the body may create a process that the session stops owning.

```ral
grant [detach: false] {
    # detach is refused; spawn and service remain session-owned alternatives
}
```

A single `false` anywhere in the active stack denies the birth. `true` cannot override an outer `false`.

When detachment is allowed, the child is created under the effective
filesystem and network projection and, on hosts that support it, the effective
executable-path projection. That projection remains attached to the survivor
for its lifetime. `detach` itself is not part of the OS profile; it decides
whether the survivor may be born.

The `detach` command is present only when the host installs it together with a birth budget.

### 12.7. `audit`

`audit: true` asks ral to record capability decisions in an audit collection
that is already active. It does not start collection by itself. Collection may
come from the language-level `audit { ... }` form or from batch `--audit`.

When both conditions hold, ral emits separate `capability-check` observations
for `exec` and ral-owned `fs` decisions, including denials. A check is a leaf
with no arguments, output, error, or children. Its `resource` is `exec` or
`fs`, its `decision` is `allowed` or `denied`, and those fields appear both in
its `value` and at top level. Exec observations identify the command; a
point-of-use check also carries its evaluated arguments and may include its
resolved name, while an early head denial has no arguments yet. Filesystem
observations identify the operation and path, and an allowed observation may
include the granting prefix.

`grant` is transparent to the simplified audit model: it does not contribute
a wrapper node. Capability checks and the body's builtin, external, or bundled
command calls appear directly in the nearest language-level audit report or
synthetic batch run root. Nested grants do not introduce additional tree
levels.

Entering a newly composed capability frame can also emit a `deputy` / `flagged`
capability observation when an executable directory prefix is writable, since
the body could replace or create a program there. Its `prefixes` field lists
the overlapping regions. This finding reports the confused-deputy shape; it
does not deny it.

Audit enablement accumulates across nested grants: once any active layer has
`audit: true`, inner layers continue to emit eligible checks even if they omit
the flag. The `net`, `detach`, `editor`, and `shell` Boolean gates do not emit
individual capability-check observations.

### 12.8. `editor` and `shell`

`editor` controls the private line-editor interface:

```ral
editor: [
    read: true,
    write: false,
    tui: false,
]
```

Its fields are:

- `read` — inspect editor text, cursor, history, keymaps, parse state, and related state;
- `write` — change editor text or state and accept input;
- `tui` — open the editor TUI facility.

`shell` currently controls working-directory changes:

```ral
shell: [chdir: false]
```

When `chdir` is false, `cd` is denied. These booleans are intersected independently across nested grants.

### 12.9. Concurrency and inheritance

A spawned worker receives a snapshot of the complete dynamic context, including the active grant stack. It cannot observe a later widening in its creator. Pipeline-stage children likewise inherit the active capabilities through their process boundary.

Authority does not flow back from a worker. A worker may narrow its own authority further, but finishing that inner grant does not change its creator’s stack.

External and bundled commands receive an OS-sandbox projection folded from the effective `fs`, `net`, and, where supported, `exec` policies at the moment of launch.

### 12.10. The two enforcement layers

ral uses two complementary enforcement layers:

1. The in-process gate checks command admission, subcommands, ral-owned filesystem operations, editor operations, `cd`, and detached-process birth.
2. The OS sandbox confines filesystem and network actions performed directly by a spawned program. On macOS it also confines executable paths.

The in-process command gate applies on every host, including when no OS sandbox is needed. It cannot see a command that an admitted child launches internally. Conversely, an OS sandbox cannot express first-argument subcommand rules and cannot protect operations ral performs in its own process.

Dynamic loader injection variables such as `LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, and `DYLD_*` are removed from child environments while capabilities are active. Confined Unix children also receive resource limits; Windows confines process trees with a Job Object.

### 12.11. Host differences and limits

**macOS.** ral launches confined children under Seatbelt. Filesystem, offline-network, and executable-path restrictions are kernel-enforced. The executable allow-list also constrains programs launched internally by an admitted child. Subcommand restrictions remain an in-process check on the original invocation.

**Linux.** ral uses bubblewrap for filesystem restrictions and `net: false`; supported architectures also receive a seccomp filter for selected dangerous syscalls. Bubblewrap has no path-based executable filter. ral therefore checks the command it launches, but an admitted program may execute another visible program internally. Pure `exec` attenuation does not by itself create a bubblewrap sandbox.

**Windows.** ral uses a projection-specific AppContainer token, filesystem capability SIDs, and a Job Object. AppContainer is deny-by-default: a child confined only to obtain `net: false` does not automatically retain ordinary access to the user’s working tree. Windows has no path-based executable filter, so internally launched executables have the same limitation as on Linux. Network and UNC grant paths are unsupported. Filesystem capability entries are attached to NTFS objects; same-volume renames and hard links can therefore preserve or omit authority differently from a purely path-based rule.

On another host, a filesystem restriction fails when no per-command sandbox backend can be built, and `net: false` is explicitly rejected if kernel network enforcement is unavailable.

### 12.12. Capability profiles

A capability profile is an ordinary `.ral` script whose terminal value must be
a `Map` with exactly the same contract as the map accepted by `grant`:

```ral
# read-only.ral
return [
    exec: [
        git: ['status', 'diff'],
        rg: 'allow',
    ],
    fs: [
        read: ['cwd:'],
    ],
    net: false,
    detach: false,
]
```

Load one or more session-wide profiles with:

```text
ral --capabilities read-only.ral script.ral
ral --capabilities base.ral,offline.ral script.ral
```

The loader parses, elaborates, type-checks, and evaluates each profile, then
decodes that terminal value. Bindings created by the script are not projected
into a policy as `use` bindings would be: the value itself must be the map.
`return [...]` is the direct way to make that contract explicit. A profile
that ends in `Unit`, a scalar, a list, or any other non-map value is rejected.

All listed profiles use the same load-time home and working directory. They
are decoded by the same strict decoder as inline grants. Each decoded profile
already contains frozen paths; the profiles are then intersected from left to
right and installed as a session-wide ceiling. Later inline grants may narrow
that ceiling but cannot widen it.

`source 'profile.ral'` instead returns the profile’s value, which can be used dynamically:

```ral
grant !{source 'profile.ral'} {
    …
}
```

Compilation or runtime failure in a profile, a non-map terminal value,
malformed maps, wrong Boolean types, non-string path or subcommand entries,
empty subcommand lists, unknown fields, unresolved required sigils, and invalid
relative paths are reported as configuration errors. Inline-map decode errors
occur before the `grant` body runs; session-profile errors prevent the requested
session or batch program from starting.

## 13. Audit, diagnostics, and testing

ral keeps three kinds of evidence distinct:

- a runtime error is a value-like description of failed computation;
- a diagnostic explains that failure to a person, usually against source text;
- an audit tree records what ran, what it returned, and which bytes crossed
  its file descriptors.

The distinctions matter. A command may write arbitrary bytes to fd 2 without
failing. A command may fail without writing any bytes to fd 2. ral's explanation
of a failure is therefore not inserted into the command's captured `stderr`;
it lives in an error record, a diagnostic, or an audit node's separate `error`
field.

Byte pipes and value pipes preserve the same separation. A byte pipe transports
bytes between stages. A value pipe transports typed values between stages.
Neither is an implicit diagnostic or audit channel.

### 13.1. Runtime errors

A runtime error has a message, a nonzero status, an optional source span, and
an optional hint. It aborts the current computation until a construct such as
`try` handles it.

`fail` raises an error from an error record:

```ral
fail [status: 7, message: 'index is stale']
```

The `status` field is required, must be an `Int`, must fit the process exit-code
range, and must be nonzero. `message` is optional and may be a `String` or
`Bytes`; absent or ill-typed messages become `explicit failure`. Extra fields
are allowed and ignored by `fail`, which permits a caught error record to be
re-raised directly:

```ral
try {
    refresh-index
} { |error|
    log $error[message]
    fail $error
}
```

Other argument shapes are errors. In particular, `fail 7` and `fail 'broken'`
do not abbreviate an error record.

`try` passes its handler a closed record:

```text
[
    cmd:     String,
    status:  Int,
    message: String,
    line:    Int,
    col:     Int,
]
```

`cmd` names the last failing command in the recorded subtree when one exists,
and is `<runtime>` when no command can be identified. `line` and `col` are
one-based source coordinates, or zero when no position is available. The
record deliberately contains no output bytes. The failed command's raw fd 1
and fd 2 bytes have already followed their ordinary destinations; use `audit`
when those bytes must also be retained as evidence.

Static checking rejects a literal `fail [status: 0]`. A dynamically computed
zero is rejected at runtime with a suggestion to use `return` for a clean
result.

Errors raised by the evaluator use the same path as explicit failures. For
example, a `case` over a record literal is checked for exhaustiveness, but the
handler table may instead be an opaque computed record. If that record lacks
the scrutinee's tag at runtime, the miss is an ordinary runtime error naming
the absent variant and the tags the table does handle. It is catchable by
`try` and recordable by `audit`; it is not an implicit `Unit`, sentinel, or
process crash.

### 13.2. Human diagnostics

The `ral` executable reports parse, type, and runtime failures on standard
error. A diagnostic normally includes:

- a stable category code;
- the source name and a highlighted source range;
- a concise message and label;
- a secondary range or help text where it clarifies the repair.

Lexer diagnostics use `L` codes, parser diagnostics use `P` codes, type
diagnostics use `T` codes, and runtime diagnostics use `R` codes. When a span
cannot be resolved safely, ral prints the code, message, and optional help
without inventing a caret in the wrong file. Source spans count bytes
internally; diagnostics convert them to character positions before rendering.

For a one-command interactive input, a compact runtime diagnostic is often
more useful than pointing back at the line just entered. It has this general
shape:

```text
error: MESSAGE (exit status N)
hint: REPAIR
```

The status suffix is omitted when the failure message already describes a
process failure, and the hint line appears only when a hint exists. Errors in
scripts, startup files, aliases, and other stored sources use the source-aware
form instead.

Colour is presentation, not meaning. It is disabled when the terminal cannot
support it or the usual no-colour conditions apply.

Batch-mode exit status distinguishes static phases:

| Failure | Exit status |
|---|---:|
| lexing, parsing, or elaboration | 2 |
| type checking | 1 |
| runtime error or explicit `exit` | the resulting status, clamped to 0–255 |

Successful execution returns the shell's final status. Diagnostics are not
values and do not flow through byte pipes or value pipes.

### 13.3. Execution trees

`audit { body }` evaluates `body` in a fresh lexical audit collection and
returns a structural report:

```ral
let report = audit {
    echo 'checking'
    verify-index
}

echo $report[status]
echo $report[children][0][cmd]
```

Auditing is observational. Captured output is teed into the tree while the
same bytes continue to their ordinary fd 1 and fd 2 destinations. `audit`
does not turn a byte pipe into a value pipe, or a value pipe into a byte pipe.
Process-staged pipeline fragments are transported back and merged into the
surrounding collection; crossing a process boundary does not make a stage
disappear from the nearest real audit parent.

The report is not itself an execution-call node. It has four fields:

```text
[
    status:   Int,
    value:    A,
    error:    String,
    children: [AuditNode],
]
```

`status`, `value`, and `error` describe the audited body's outcome. `error` is
the empty string on success. `children` contains the execution calls and
capability observations made during the body's dynamic extent.

Each child observation has this common shape:

| Field | Type | Meaning |
|---|---|---|
| `kind` | `String` | `command` or `capability-check` |
| `cmd` | `String` | builtin, external or bundled command, resource, or run name |
| `args` | `[String]` | evaluated arguments rendered as strings, where applicable |
| `status` | `Int` | the status observed for the node |
| `script` | `String` | source or run name |
| `line` | `Int` | one-based source line; zero for a run root |
| `col` | `Int` | one-based source column; zero for a run root |
| `stdout` | `Bytes` | raw bytes observed on the node's fd 1 |
| `stderr` | `Bytes` | raw bytes observed on the node's fd 2 |
| `error` | `String` | ral's runtime error message, or the empty string |
| `value` | any | returned value, or `Unit` on failure |
| `children` | list of records | nested observations; empty on ordinary call nodes |
| `start` | `Int` | microseconds since the Unix epoch |
| `end` | `Int` | microseconds since the Unix epoch |
| `principal` | `String` | shell principal when the node was recorded |

Every child observation contains every common field. This does not apply to
the four-field structural report itself. An observation's `error` field is
present even on success, where it is the empty string. It is populated only
from ral's runtime-error path; it is not copied from `stderr`, and a nonzero
status need not imply a nonempty `error`. Conversely, `stderr` contains only
bytes observed on fd 2. It never contains synthetic runtime prose merely
because a command failed.

Captured `stderr` is limited to the first 64 KiB per node. There is no
truncation marker. Captured `stdout` has no corresponding audit-node cap.
These are retention rules, not I/O limits: all bytes still stream to their
ordinary destination.

Execution-call frames cover public builtins, external commands, bundled
commands, and pipeline stages. Arguments are recorded after evaluation and
rendered to strings. Internal builtins whose names begin with `_` are omitted,
so implementation details beneath a public wrapper do not masquerade as user
commands.

User-function application is transparent. Control forms and scopes, including
`if`, `case`, `within`, `grant`, `guard`, and `try`, are transparent too.
Collection iteration does not manufacture a wrapper node. The public builtin
call that performs an iteration, such as `each` or `map`, is still a real
builtin call and is recorded once under the ordinary rule.

Commands executed inside a transparent function, form, scope, or iteration
remain visible. They attach directly to the nearest real audit parent rather
than to a synthetic wrapper. A function that only computes and returns a value
may therefore add no child node at all. A nested `audit` returns its own report,
while its execution-call observations also merge directly into an enclosing
audit collection.

`try` always forces enough collection to identify a failed command, but it
does not request byte retention by itself. Inside an enclosing `audit`, byte
capture remains enabled: an inner `try` cannot silence an outer audit. `try`
still returns the body or handler result and does not add a `try` node.

`audit` handles an ordinary runtime error as data. Its returned report carries
the failure status and message, and evaluating the `audit` expression itself
succeeds with that record. It also sets `$STATUS` to the recorded status.
Control escapes are different: `exit` and a stopped computation propagate out
instead of being converted into a returned audit report.

#### Capability checks

Capability checks are recorded only when an audit trail is active and an
enclosing capability grant requested `audit: true`. They use
`kind: 'capability-check'`, are leaves, contain no arguments or I/O, and have
equal `start` and `end` timestamps.

Their `value` contains at least `resource` and `decision`, together with the
resource-specific fields. Those fields are also copied to the node's top
level for direct inspection. A denied decision has status 1; other decisions
have status 0. A denial does not manufacture raw `stderr` bytes.

### 13.4. Batch audit JSON

For a script or `-c` program, `--audit` records the entire run and emits a JSON
tree after execution:

```text
ral --audit script.ral
ral --audit --pretty -c 'echo hello'
```

`--pretty` requires `--audit`. These flags are batch options for an explicit
script or `-c`; they are not interactive or stdin-script options.

Unlike the four-field record returned by the language-level `audit` form, the
top-level JSON object is a full synthetic run node. It has the common execution
node shape, `cmd` and `script` equal to the run name, no arguments or I/O of its
own, and source position 0:0. Its real `start` and `end` timestamps bound the
run. Its children are the run's collected observations. Its status is the
final process status chosen by the host. An ordinary runtime failure fills its
`error`; an explicit `exit` is a control escape and leaves `error` empty.

The JSON projection favours legibility over round-trip fidelity:

- `Bytes` become strings using lossy UTF-8 conversion;
- `Unit` becomes `null`;
- non-finite floats become `null`;
- variants become objects with `tag` and, where present, `payload`;
- executable or process-local values such as blocks, functions, natives, and
  handles become descriptive stubs.

Compact mode writes one JSON document followed by a newline. Pretty mode
writes the same data with indentation. Both write to fd 2. Program output is
not suppressed: fd 1 remains ordinary stdout and raw program fd 2 bytes remain
ordinary stderr. Consequently the JSON shares stderr with any bytes the
program writes there, and with optional `RAL_TIMING` lines. fd 2 is not
promised to be a standalone JSON stream.

On a runtime failure, `--audit` suppresses the separate human diagnostic
because the root's `error` already carries ral's explanation. It does not
suppress the program's raw stderr. Parse, elaboration, and type errors happen
before execution, so they produce their ordinary diagnostics and no audit
tree. The program's real exit status is preserved; `--audit` does not turn a
failed run into a successful one.

### 13.5. Testing ral programs

The language does not reserve a privileged assertion form. Tests can build
small assertions from `equal`, `fail`, and ordinary functions:

```ral
let assert-equal = { |name expected actual|
    if $[not !{equal $expected $actual}] {
        fail [
            status: 1,
            message: "assertion failed: $name",
        ]
    }
}

assert-equal 'answer' 42 !{compute-answer}
```

Expected failures remain explicit:

```ral
try {
    parse-config 'broken input'
    fail [status: 1, message: 'parse unexpectedly succeeded']
} { |error|
    assert-equal 'parse status' 2 $error[status]
}
```

`within [handlers: ...]` supplies deterministic command doubles without
changing global process state:

```ral
within [handlers: [fetch-clock: { |args| return '12:00' }]] {
    assert-equal 'clock' '12:00' !{fetch-clock}
}
```

The handler receives one list containing the command's evaluated arguments.
Per-name handlers follow the ordinary command-dispatch rules, including inside
pipelines using byte pipes. A value pipe remains a typed function application, so tests
should mock its functions as values rather than pretending it is a byte
command channel.

`audit` is the execution-observation testing surface. It can assert the
observation sequence, statuses, values, raw output, capability decisions, and
source locations without scraping a human diagnostic. Transparent language
forms do not supply nesting assertions; their nested calls appear directly in
the nearest report. Tests that compare output bytes should inspect `stdout` or
`stderr`; tests that compare ral's explanation should inspect `error` or a
caught error record's `message`.

For executable-level checks:

- `ral --check` or `ral -n` parses, elaborates, and type-checks a script
  without executing it;
- `ral --dump-ast` parses and prints the debug AST to stderr without
  elaborating, type-checking, or executing it;
- `ral --audit` exercises the real program and exposes its execution tree.

The repository's script harness discovers `tests/**/*.ral`, runs each runnable
script through the real `ral` executable, and requires a zero exit status.
For the portable deterministic subset, stdout is compared with a sibling
`.out` file after ignoring carriage returns. `RAL_BLESS=1` rewrites those
goldens; maintainers should bless with the `grep,ripgrep` feature set so the
regex-gated scripts are included:

```text
RAL_BLESS=1 cargo test -p ral --features grep,ripgrep --test scripts
```

Those harness conventions test this implementation; they are not additional
language semantics.

## 14. Standard environment

Every normal ral program starts with two layers of public names:

- **core builtins**, implemented by the runtime for primitive value operations,
  shell state, structured operating-system queries, codecs, and concurrency;
- the **prelude**, an ordinary ral module loaded before user code and built from
  those primitives.

An embedding host may add a third layer. The ral REPL adds interactive commands;
exarch adds agent tools and a small helper library. Host additions are not part
of the portable core environment.

The standard environment is typed. A builtin that consumes or produces bytes
declares that in its type, so the typechecker distinguishes its byte pipes from
value pipes before execution.

### 14.1. Discovering the live environment

The running shell is the authority on the names it actually provides:

```ral
help
explain map
explain from-json
```

`help` prints sorted `Builtins:` and `Prelude:` sections. It uses the builtin
registry and the prelude's `##` documentation, the same sources used to build
the shell. When a host has installed documentation for a sourced library, help
also prints a `Library:` section.

`explain name` prints the name's summary, full inferred type or command
signature, and where the name resolves. It understands core and host builtins,
prelude entries, documented host libraries, locals, aliases, handlers, and
external commands. If no exact documented name exists, it searches documented
names case-insensitively. Regex search is used when regex support is compiled
in; otherwise the search is a substring match.

`help` is the canonical catalogue of documented standard and host-library
entries, not a list of every command that might be reachable. Arbitrary aliases,
plugin bindings, and programs found on `PATH` are dynamic. Use `explain name`
to inspect one of those.

Names beginning with `_` are implementation interfaces. They are hidden from
`help` and are not part of the public scripting contract. Prelude code and
plugins may use the particular private interfaces supplied for them; ordinary
programs should not.

### 14.2. Values, collections, and comparison

The primitive collection family includes:

- `each`, `map`, and `filter` for applying a function to list elements;
- `fold` for a left-to-right reduction;
- `sort-list` and `sort-list-by`;
- `range start end`, whose end is exclusive;
- `length`, `is-empty`, `keys`, and `has`;
- `equal`, `lt`, and `gt`.

```ral
let squares = range 0 5 | map { |n| return $[$n * $n] }
let names = keys $record
```

These are value operations: ordinary composition uses value pipes. Higher-order
operations stop and propagate an error if their callback fails. Lists remain
homogeneous, and the typechecker rejects a callback or accumulator with an
incompatible type.

`equal` is structural. `lt` and `gt` compare numbers numerically and strings
lexicographically; unsupported or incompatible shapes are type errors. The
predicate family returns `Bool`. A result of `false` is a successful
computation, though it records status 1 for shell-style status inspection.

The prelude builds the familiar derived operations from these primitives:
`for`, `reduce`, `reverse`, `last`, `take`, `drop`, `take-while`, `drop-while`,
`first`, `option-or`, `elem`, `contains`, `nub`, `zip`, `flat-map`, `enumerate`,
`concat`, `sum`, `cross`, `group-by`, `median`, and `median-by`.

### 14.3. Strings, numbers, and shell text

The string family includes `upper`, `lower`, `dedent`, `slice`, and
`intercalate`. `string-replace` performs one literal replacement and fails
unless there is exactly one match. Regex operations use the explicit `re-`
prefix: `re-match`, `re-split`, `re-find-match`, `re-find-matches`,
`re-replace`, and `re-replace-all`.

`shell-quote` produces a safely quoted shell word; `shell-split` parses shell
word syntax. They are explicit conversions, not an invitation for ral values to
be re-lexed automatically.

`int`, `float`, and `str` convert scalar values. `round x places` returns a
`Float`, rounds halves away from zero, and accepts 0 through 308 decimal places.
`floor`, `ceil`, and `trunc` accept a finite, in-range `Float` and return an
`Int`; an `Int` is already integral and is rejected at the type level.

The prelude adds `lines`, `words`, and `indent`. It also supplies `ansi-reset`,
`ansi-bold`, `ansi-dim`, `ansi-red`, `ansi-green`, `ansi-yellow`, `ansi-blue`,
`ansi-magenta`, `ansi-cyan`, and `styled`. These are the complete public ANSI
constant set; black, white, underline, reverse, and bold-colour variants are
not prelude names. Every exported constant is an empty string when `NO_COLOR`
is active, stdout is not a terminal, the selected interactive mode is minimal,
or stdout does not support ANSI colour. `styled code text` concatenates the
code, text, and `ansi-reset`, so it becomes plain text under the same gate.

Regex names remain present in builds without regex support so source-level name
resolution stays stable, but calling one then fails with a clear feature error.

### 14.4. Paths and filesystem queries

ral provides structured queries rather than requiring programs to parse the
text output of `ls` or `stat`:

- `cwd`, `absolute-path`, and `resolve-path`;
- `list-dir`, `file-info`, and `glob`;
- `exists`, `is-file`, `is-dir`, `is-link`, `is-readable`, and `is-writable`;
- `temp-dir` and `temp-file`.

`absolute-path` is lexical: it resolves `~`, `.`, and `..` against the logical
working directory without requiring the result to exist. `resolve-path`
canonicalises through the filesystem, follows symbolic links, and requires an
existing path. Filesystem reads and creations are checked against the active
filesystem grant.

`list-dir` returns records rather than formatted columns. `file-info` uses
link metadata and reports fields including name, type, size, timestamps,
readonly state, and symbolic-link target. An unavailable timestamp is 0.

The prelude adds `file-empty`, `line-count`, and `from-lines-list`.
`from-lines-list` refuses files larger than 10 MiB because it materialises the
whole result. For bounded-memory processing, redirect the file into
`fold-lines`, `map-lines`, `filter-lines`, or `each-line`.

Filesystem mutations deliberately use command-shaped tools such as `cp`, `mv`,
`rm`, `mkdir`, and `ln`. They are effects with byte-oriented command
interfaces, not duplicate structured builtins.

### 14.5. Bytes, text, and streams

The codec families are the named boundary between values and bytes:

```ral
cat data.json | from-json
$record | to-json > data.json
```

The core environment provides `from-bytes`, `from-string`, `from-line`,
`from-lines`, `from-json`, and `from-csv`, with corresponding `to-` encoders.
Codecs do not silently reinterpret arbitrary values or output. Their exact
UTF-8, newline, JSON, CSV, and capture rules are specified with pipelines and
input/output.

`fold-lines` is the bounded-memory primitive for a byte pipe of UTF-8 lines. It
removes each line terminator, calls `fn accumulator line`, and returns the final
accumulator. It does not first build a list, though the accumulator and callback
may of course retain data themselves.

The prelude builds three bounded-memory filters on that primitive:

- `map-lines f` calls `f` for each line and echoes the returned value as one
  output line;
- `filter-lines predicate` echoes the original line when the predicate returns
  `true`;
- `each-line f` calls `f` and discards its returned value, while any bytes the
  callback itself writes continue through the active byte pipe.

Use them with a byte pipe or redirect, for example
`map-lines $clean < input.txt`. Calling a line reader with terminal stdin and
no pipe or redirect is an error rather than an interactive prompt.

The prelude also provides the value-stream constructors and consumers
`stream-cons`, `stream-nil`, `stream-take`, `stream-drop`, `stream-map`,
`stream-fold`, `stream-each`, and `stream-to-list`.

`from-lines` and `from-jsonl` currently read their complete byte input before
returning their stream value. Their later transformations may be lazy, but the
initial read is not a bounded-memory operation. `to-jsonl` writes one compact
JSON value per line.

### 14.6. Failure, session control, and concurrency

The core session family includes `fail`, `exit` and `quit`, `cd`, `alias` and
`unalias`, `source` and `use`, `ask`, `echo`, `surface`, `clear`, `reset`,
`help`, and `explain`.

`fail` raises a recoverable failure from exactly one error record:

```ral
fail [status: 7, message: 'unavailable']
```

The `status` field is required, must be an `Int` in the supported exit-code
range, and must be nonzero. `message` is optional and may be a `String` or
`Bytes`; it defaults to `"explicit failure"`. Additional fields are ignored,
which lets `fail $err` re-raise the record supplied by `try`. Passing a bare
integer, string, bytes value, a record without an integer `status`, or status 0
is an error. `exit` and `quit` leave the current program rather than producing
a recoverable failure.

`surface value` sends a first-order structured event to a host that installed a
surface sink; without one, it safely produces no visible event. `ask` reads from
the controlling terminal rather than redirected standard input and fails on
end-of-file.

Concurrency primitives are `spawn`, `await`, `poll`, `race`, and `cancel`.
They operate on typed handles. The prelude adds `par`, `is-done`, `attempt`,
`succeeds`, and `defer`. Exact ownership, cancellation, output capture, and
lifetime rules are specified with concurrency.

### 14.7. Host and build additions

The portable contract is the core registry plus the prelude. A host may install
more names before user code is typechecked; those names then appear in that
shell's `help` and `explain` output.

The ral command-line host provides `watch`. Its interactive form also supplies
job-control and plugin-lifecycle commands and private editor interfaces. A
plugin may install validated aliases, hooks, and keybindings; those are session
extensions, not portable standard names.

Exarch installs its agent operations, the durable `service` operation, and a
documented ral helper library. It does not provide `watch`. On supported Unix
hosts it may also provide `detach`, whose process deliberately outlives ral and
cannot be awaited or cancelled through a handle. These host-owned names must
not be assumed in a portable ral script.

Builds may bundle external-style utilities. The `coreutils` feature provides a
curated cross-platform set including `ls`, `cat`, `wc`, `head`, `tail`, `cp`,
`mv`, `rm`, `mkdir`, `sort`, and related tools. Unix builds may add `id`,
`kill`, `stat`, `tac`, `test`, and `timeout`; separate features add `cmp`,
`diff`, or `rg`. A bundled bare name wins over a program of the same name on
`PATH`, but it still runs as a child executable image with the current cwd,
environment, byte pipes, redirects, status, and capability restrictions.

The standalone `ral` build does not guarantee bundled coreutils; ordinary
external lookup remains available. Exarch enables its bundled tool set for a
more self-contained environment. Platform-gated tools are absent rather than
registered as commands that can never run.

## 15. Interactive use and extensions

Interactive editing is a service of the `ral` program, not part of the core
language. A portable ral program may rely on values, commands, byte pipes,
value pipes, types, and the core builtin surface. It must not assume that its
host has a prompt, history, completion, job control, plugins, or editor
builtins.

The `ral` executable adds those facilities when it creates an interactive
session. It installs their builtin signatures before checking startup files or
prompt input, so the type checker sees the exact surface that the session can
execute. Another host may install a different surface or none at all.

### 15.1. Entering the interactive shell

With no script or `-c` argument, `ral` starts an interactive session when
standard input is a terminal. Otherwise it reads standard input to end of file
and runs it as a batch script.

```text
ral                         # interactive when stdin is a terminal
ral -i                      # force an interactive session
ral -s                      # read a batch script from stdin
ral --surface minimal       # choose the canonical-input frontend
ral --surface readline      # choose the full line editor
ral --surface structural    # request the structural frontend
ral --norc                  # skip every startup file
```

`-s` takes precedence over `-i`. A positional script takes precedence over
both. `--surface` overrides the RC setting of the same name.

The shell collects lines while the parser reports incomplete syntax. It joins
continuation lines with newline characters and evaluates the result as one
top-level run. Abandoning a continuation discards the whole partial input. A
parse error, type error, or runtime error is reported and the next prompt is
shown; `exit` ends the session.

The interactive evaluator is the ordinary language evaluator. In particular,
a byte pipe still transports bytes and a value pipe still transports typed
values. The frontend does not reinterpret either kind of pipe.

After a successful run, `Unit` prints nothing and `Bytes` are written without a
prefix. Other values are displayed with the REPL renderer. Lists and maps use
the bounded pretty-printer; other values use their ordinary display form. The
default prefix is `=> ` and the default value colour is yellow where colour is
supported.

### 15.2. Frontends

The `ral` executable provides three interactive frontends:

| Surface | Contract |
|---|---|
| `minimal` | Canonical standard-input reads and a `> ` continuation prompt; no raw mode, completion, editor keybindings, ghost text, or highlighting. |
| `readline` | The default full editor, with history, completion, vi or Emacs editing, plugin keybindings, ghost text, and highlighting. |
| `structural` | A feature-gated terminal projection of the same session, including typed bindings, worksheet information, and handles. It uses the same completion and plugin dispatch rules as `readline`. |

Terminal capability is stronger than preference. A dumb terminal, or
`RAL_INTERACTIVE_MODE=minimal`, forces the minimal surface even when another
surface was requested. If `structural` is requested but the binary lacks that
feature or the terminal cannot enter raw mode, ral warns and falls back to
`readline`.

The full frontends complete variables, command-position names, and paths.
Variable candidates come from live non-private bindings. Command candidates
also include installed builtins, handlers, and executables on `PATH`. Relative
paths are resolved from ral's logical working directory. Matching is fuzzy;
ties are stable and alphabetical. A new binding is visible at the next prompt.

History is persisted in the ral configuration directory, with a legacy
`~/.ral_history` fallback. Consecutive duplicate entries are omitted. Saving
appends the current session's new entries instead of replacing the file, so
concurrent sessions do not erase each other's history.

In a full frontend, Ctrl-C at the prompt abandons the current input and redraws;
Ctrl-D on an empty prompt exits cleanly. Ctrl-D within non-empty input performs
the editor's ordinary delete operation. Ctrl-C while a command is running
interrupts that foreground run. Unix Ctrl-Z and job-control behaviour are
specified with concurrency and jobs; a pure value pipe has no process group to
stop. Windows has no Unix stopped-job path.

### 15.3. Startup files and RC configuration

A login session reads these optional profiles in order:

1. `/etc/ral/profile`
2. `~/.ral_profile`

Profiles are ordinary ral scripts sourced for their effects. Their file-level
contract is strict: they must return `Unit`. A map, scalar, list, or any other
value is rejected with a diagnostic; configuration maps belong in the RC file.

Every interactive session then reads the first existing RC file:

1. `$XDG_CONFIG_HOME/ral/rc`, normally `~/.config/ral/rc`
2. `~/.ralrc`

If neither exists, ral attempts to create a documented skeleton at the first
available location. `--norc`, also accepted as `--noprofile`, skips both login
profiles and the RC file.

The RC file is ordinary ral source and its file-level result must be a `Map`.
`Unit` and every other result shape are rejected rather than treated as an
empty configuration. For example:

```ral
return [
    edit_mode: vi,
    bell: false,
    surface: readline,
    recursion_limit: 1024,

    prompt: { return "$CWD ❯ " },
    env: [EDITOR: 'vim', PAGER: 'less'],
    bindings: [work: '/srv/work'],
    aliases: [ll: { |args| ls -lh ...$args }],
    theme: [value_prefix: '⇒ ', value_color: cyan],
    startup: { echo 'ready' },
]
```

The recognized fields are:

| Field | Meaning |
|---|---|
| `env: Map` | Set environment entries and bindings. `PWD` and `OLDPWD` are ignored because ral derives them from its logical working-directory state. |
| `prompt: Block` | Install the zero-argument base-prompt body. |
| `bindings: Map` | Install lexical values, including functions. |
| `aliases: Map` | Install block or function values as command aliases; other values become ordinary lexical bindings. |
| `edit_mode: String` | `emacs` or `vi`; default `emacs`. |
| `bell: Bool` | Enable or disable the audible line-editor bell; default `false`. |
| `surface: String` | `minimal`, `readline`, or `structural`; default `readline`. |
| `recursion_limit: Int` | A positive function-call recursion limit; default `1024`. |
| `plugins: List` | Plugins to load before the first prompt; see below. |
| `startup: Block` | A zero-argument block run once after the map is applied. |
| `theme: Map` | `value_prefix: String` and `value_color: String`. The colour is one of `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white`, or `none`. |

Unknown top-level RC keys are ignored for forward compatibility. An unknown
key inside `theme` or an RC plugin entry produces a warning. Every recognized
field is shape-checked without stringification or coercion: a wrong type or
invalid value produces a diagnostic naming the field and expected shape, while
the remaining top-level fields still apply.

The prompt body may return a value, whose display form becomes the prompt. If
it returns `Unit`, its captured standard output becomes the prompt, with one
trailing newline removed. Prompt failures are reported and fall back to `❯ `.
Plugin prompt hooks then transform the base prompt in plugin load order.

Parse, type, read, and runtime failures in a startup file are reported without
preventing the interactive shell from starting. `exit` stops the current
startup file and boot continues. Startup files cannot park Unix jobs: a stop
that escapes an RC or startup block is reported.

RC and profile evaluation is trusted session bootstrap. Command-line
`--recursion-limit`, `--surface`, and capability profiles are applied
afterward, so the command line wins and the session capability ceiling narrows
the resulting shell.

### 15.4. Discovering the installed surface

`help` lists the installed public builtins, documented prelude bindings, and
any library documentation supplied by the host. Names beginning with `_` are
hidden. The lists are sorted.

`explain name` shows the entry's documentation, inferred or declared type, and
where name resolution would find it. When no exact entry resolves, `explain`
performs a case-insensitive search and prints matching public entries. These
commands are informational: a missing query prints a message and leaves status
zero.

Because help is assembled from the live shell, it describes host additions
such as `jobs` and `load-plugin` only where they are actually installed.

### 15.5. Plugins

A plugin is an ordinary ral file. Its file-level result is either a manifest
`Map` directly or a factory taking exactly one options `Map` and returning that
manifest. No other result shape is accepted.

```ral
let change = { |event|
    let line = $event[line]
    if $[not !{is-empty $line}] { _ed-ghost ' …' } else { _ed-ghost '' }
}

return { |options|
    let key = get $options key 'ctrl-g'
    return [
        name: 'example',
        hooks: [buffer-change: $change],
        keybindings: [[
            key: $key,
            handler: { |_event| _ed-insert 'hello' },
        ]],
        aliases: [hi: { |args| echo 'hello' ...$args }],
    ]
}
```

The manifest schema is:

```text
[
    name: String,
    hooks?: Map<String, Block>,
    keybindings?: List<[key: String, handler: Block, guard?: String]>,
    aliases?: Map<String, Block>,
]
```

`name` is required. The other fields default to empty collections. Each
declared field is checked exactly: ral does not stringify a value of the wrong
type or silently drop a malformed handler. Hook and keybinding handlers each
take exactly one argument. Unknown hook names, wrong handler arity, invalid key
notation, invalid guard regexes, and alias conflicts are load errors.
Validation and registration are atomic; a rejected load leaves no hooks,
keybindings, or aliases installed.

Plugins run with the authority of the live session and current grant stack. A
manifest `capabilities:` field is rejected because it would not enforce
confinement. Use `grant` around an invocation that needs narrower authority.
Plugin options must be a first-order map: they cannot contain blocks, handles,
or other non-transportable values.

`load-plugin name-or-path` searches in this order:

1. the `ral/plugins` subdirectory of the user configuration directory;
2. each directory in `RAL_PATH`;
3. the literal path;
4. the literal path with `.ral` appended.

The interactive command supplies an empty options map to a factory. A direct
manifest rejects non-empty options. To configure a factory, load it from the RC
file:

```ral
return [
    plugins: [
        [plugin: 'autosuggestion'],
        [plugin: 'fzf-files', options: [key: 'ctrl-t']],
    ],
]
```

Each RC plugin entry requires `plugin: String`; `options`, when present, must
be a map. `unload-plugin name` removes that plugin's hooks, keybindings, and
aliases. Loading a duplicate plugin name or unloading a name that is not
loaded is an error.

Plugin aliases occupy the alias namespace. A collision with an existing alias
is a load error. A lexical or native command with the same name is permitted;
ordinary name resolution decides which bare head wins, and `^name` reaches the
alias while skipping lexical and native lookup.

### 15.6. Hook and keybinding events

Every plugin hook takes exactly one argument, and registration checks this
arity. In particular, lifecycle fields are not passed as separate positional
arguments: `pre-exec`, `post-exec`, and `chpwd` each receive one event record.
The recognized hooks are:

| Hook | Argument and result | Time |
|---|---|---|
| `buffer-change` | `[old_buf: String, line: String, pos: Int, history: List<String>, keymap: String, state: Any]`; returns `Unit` | After the text or cursor changes. |
| `pre-exec` | `[src: String]`; returns `Unit` | Before one complete prompt input is evaluated. |
| `post-exec` | `[src: String, status: Int]`; returns `Unit` | After evaluation settles. |
| `chpwd` | `[old: String, new: String]`; returns `Unit` | After the session working directory changes. |
| `prompt` | The current prompt `String`; returns a `String` | Before each prompt render. |

The four `Unit` result contracts above are enforced. A non-`Unit` result is a
hook failure, not an ignored value. The `prompt` hook is deliberately
different: it is a string transformer, so each successful `String` becomes the
next plugin's input.

Hooks run in plugin load order. A failed lifecycle hook is reported but does
not prevent later plugins' hooks from running or replace the command's status.
Prompt transformers compose: each successful result becomes the next hook's
input, while a failed or non-string result leaves the current prompt unchanged.

`buffer-change` runs on the editing path, so ral protects the session from a
persistently bad handler. A single invocation that exceeds 100 milliseconds,
or three consecutive failures, disables that plugin's `buffer-change` hook for
the rest of the session and prints a notice. `_ed-tui` is unavailable inside
`buffer-change`.

A keybinding handler receives:

```text
[line: String, cursor: Int, history: List<String>, keymap: String, state: Any]
```

Key names are `tab`, `enter`, `escape`, arrows, `home`, `end`, `delete`,
`backspace`, `f1` through `f12`, a single character, `ctrl-character`, or
`alt-character`. Ctrl-C and Ctrl-D are reserved and cannot be bound.

Keybindings form one ordered table: plugin load order, then manifest order,
then the frontend's built-in action. A `guard` is a regular expression matched
against the text left of the cursor. A binding claims its key only when the
guard matches. Every unmodified key except F1 through F12 already has an editor
action and therefore requires a guard. Modified chords and function keys may
be unguarded. A later binding hidden by an earlier unguarded binding loads with
a warning naming the earlier owner.

The minimal frontend does not dispatch buffer-change hooks or editor
keybindings and cannot display ghost text or highlights. Plugin aliases and
the `pre-exec`, `post-exec`, `chpwd`, and `prompt` hooks remain session
facilities and do not depend on raw-mode editing.

### 15.7. The editor plugin interface

The `_ed-*` builtins are installed only by the interactive `ral` host and are
hidden from `help`. Except for the pure `_ed-hyperlink` formatter, they require
the corresponding `editor.read`, `editor.write`, or `editor.tui` authority.
They also require an active plugin handler; calling one at an ordinary prompt
or from a batch host is an error.

Cursor and span offsets count Unicode characters, not UTF-8 bytes.

| Builtin | Contract |
|---|---|
| `_ed-get` | Return `[text, cursor, keymap]`. |
| `_ed-text`, `_ed-cursor`, `_ed-keymap`, `_ed-lbuffer` | Read the whole buffer, cursor, active keymap, or text left of the cursor. |
| `_ed-set map` | Replace `text` and/or `cursor`; omitted fields are unchanged and an out-of-range cursor is clamped. |
| `_ed-set-lbuffer text` | Replace text left of the cursor, preserving the right side. |
| `_ed-insert text` | Insert at the cursor and advance past the insertion. |
| `_ed-push` | Save the current buffer on a stack and clear it for another command. |
| `_ed-accept` | Execute the resulting buffer immediately when the handler returns. |
| `_ed-history prefix limit` | Return matching history, most recent first; zero means no limit. |
| `_ed-parse` | Return `[words, current, offset]` for the simple command at the cursor. |
| `_ed-ghost text` | Set suggestion text; an empty string clears it. |
| `_ed-highlight spans` | Replace highlight spans of shape `[start, end, style]`; an empty list clears them. |
| `_ed-state default updater` | Read, transform, store, and return one persistent cell belonging to the plugin. |
| `_ed-tui block` | Suspend the editor, run the block, and return `[output: String, status: Int]`. |
| `_ed-clipboard text` | Attempt an OSC 52 clipboard write; return whether the terminal accepted emission. |
| `_ed-hyperlink uri text` | Return an OSC 8 hyperlink where supported, otherwise return plain `text`. |

`_ed-push` and `_ed-accept` may be combined to preserve the current input while
running a generated command. Ghost text and highlights are display state; they
do not become part of the input buffer.

`_ed-tui` captures the body's standard output while allowing an interactive
child to use the terminal. On success, a non-`Unit` return value supplies
`output`; otherwise captured bytes are decoded as text with one trailing
newline removed. A body failure is returned as data in `output` and `status`
rather than raised. Nested `_ed-tui` calls and calls from `buffer-change` are
reported as status 1.

Terminal presentation remains capability-dependent. Clipboard emission may
return `false`; hyperlink formatting may return plain text; colour, ghost text,
and highlights may be suppressed by the selected surface or terminal. These
fallbacks do not change language evaluation.

### 15.8. Host and platform limits

The following are promises of the `ral` executable, not requirements on every
ral host:

- persisted history, completion, terminal titles, and the three frontends;
- startup profiles and RC discovery;
- plugins, `_ed-*`, `load-plugin`, and `unload-plugin`;
- `jobs`, `fg`, `bg`, and `disown`;
- terminal signal handling and crash-time terminal restoration.

Full stopped-job control depends on Unix process groups and a controlling
terminal. Windows provides console interruption but no Ctrl-Z stopped-job
table. The minimal frontend uses canonical input and offers no raw editor
surface. Batch execution and embedded hosts may omit all these facilities;
portable code must discover optional installed commands rather than assuming
them.

## 16. Invocation, interoperability, and platforms

The ral language and the `ral` executable are separate contracts. The language defines ral source, values, byte pipes, value pipes, and effects. The executable chooses where source comes from, prepares a session, reports diagnostics, and maps the result to an operating-system exit status.

Direct invocations always parse ral syntax:

```text
ral
ral build.ral release --clean
ral -c 'return 2 + 2'
ral -s < build.ral
```

`ral -c` does not accept POSIX shell syntax. Use `ral-sh` as the registered login shell when programs may call `$SHELL -c` with POSIX source.

### 16.1. Invocation modes

`ral` selects one of four modes:

- `ral` starts an interactive session when stdin is a terminal.
- `ral SCRIPT [ARG …]` reads and runs a script file.
- `ral -c CODE [ARG …]` runs inline ral source.
- `ral -s` reads ral source from stdin to EOF and runs it as a batch.

On Unix, a bare invocation with non-terminal stdin also selects stdin-script mode:

```text
generate-ral-source | ral
```

Reading source from stdin consumes that byte stream to EOF before execution. It is therefore already exhausted if the resulting program later tries to read its runtime stdin. File and `-c` modes do not consume stdin; the program may use it as a byte pipe.

`-i` forces the interactive session even when stdin is not a terminal. `-s` forces stdin-script mode and takes precedence over `-i`. A script positional takes precedence over both.

After `SCRIPT`, every remaining token is a script argument, even if it begins with `-`. After `-c`, the first token is the complete source string and every later token is an argument. ral inserts the option terminator internally, so these are equivalent:

```text
ral task.ral --force
ral -- task.ral --force
ral -c '--version'
```

Script arguments are available through `$ARGS` and the positional forms such as `$1`. `$SCRIPT` is a lexical string containing the source file’s name. It is available in script files, including sourced modules under their own names, but is rejected in the REPL, `-c`, and synthetic preloaded source.

### 16.2. Batch processing

A batch run parses, elaborates, typechecks, and only then evaluates. Typechecking is always performed because it also annotates the program with the byte-pipe and value-pipe modes used during evaluation.

The batch-only inspection flags are:

- `-n`, `--check` — parse, elaborate, and typecheck without executing;
- `--dump-ast` — parse and print the syntax tree to stderr without elaborating or executing;
- `--audit` — execute and print the audit tree as JSON on stderr;
- `--pretty` — pretty-print that JSON; it requires `--audit`.

These flags require a script path or `-c`; they are not accepted for a bare interactive or stdin-script invocation.

The checker and evaluator receive the same batch builtin surface. A command cannot pass `--check` by being treated as one kind of builtin and then run as another.

The emitted JSON has one structural root for the whole batch run. Beneath it,
call nodes exist only for public builtin calls and for external or bundled
command executions. Function application, control forms such as `if`, `case`,
`try`, `guard`, `within`, `grant`, and `audit`, and loop iterations do not add
call nodes of their own; the calls executed inside them attach directly to the
open audit trail. Each call node carries its evaluated arguments, outcome,
status, source location, timing, and principal, plus captured bytes when the
active audit capture policy requests them.

Batch stdout and stderr remain ordinary byte streams. A program’s final ral value is not an operating-system pipe protocol: value pipes exist between ral stages inside the evaluator, while external programs exchange bytes. Explicit adapters perform any conversion between values and bytes.

### 16.3. Results, diagnostics, and exit status

Parse and elaboration failures print source-labelled diagnostics and exit 2. Type errors exit 1. Failure to read the script file exits 1. A malformed or unreadable `--capabilities` profile exits 2.

At runtime:

- a normal completion returns the shell’s last command status;
- `exit N` returns `N`;
- a raised runtime error prints a source-labelled diagnostic and returns that error’s status;
- on Unix, an escaped stopped job returns 1.

The final process status is clamped to `0..=255`. With `--audit`, the runtime failure is represented in the JSON tree and the duplicate ordinary diagnostic is suppressed.

A foreground batch launched from a terminal may lend that terminal to an interactive child. A backgrounded or non-terminal batch has no such authority.

### 16.4. Interactive startup

`-l` or `--login` requests a login session. On Unix, an executable name whose basename starts with `-` does the same. Login affects only an interactive session: `ral -l -c CODE` and `ral -l SCRIPT` remain ordinary batch runs and do not source interactive startup files.

An interactive login session processes, in order:

1. `/etc/ral/profile`, if present;
2. `~/.ral_profile`, if present;
3. the ordinary rc file.

A login profile is ral source run for its effects and must return `Unit`. Returning a map or another value is reported as an error; interactive startup continues.

Every interactive session then reads `$XDG_CONFIG_HOME/ral/rc` or `~/.ralrc`. The rc file must return a configuration map. If neither file exists, ral may create a default rc skeleton and reports the created path. Parse, type, runtime, or return-contract errors are reported, no invalid configuration map is applied, and the user still receives a shell. An `exit` from a startup file stops that file without terminating the session.

Startup evaluation is not transactional. A read, parse, or type error prevents
that file from running, and an invalid RC result prevents configuration-map
application, but bindings and effects produced before a runtime failure,
`exit`, or return-contract check are not rolled back. In every case boot
continues to the interactive shell.

`--norc`, also accepted as `--noprofile`, suppresses login profiles and the rc file. Command-line `--recursion-limit` and `--surface` values are applied after the rc and therefore override it. Session `--capabilities` profiles are also applied after startup files: startup is operator-trusted setup, and the capability profile becomes the ceiling for subsequent work.

### 16.5. Interactive surfaces

`--surface minimal`, `--surface readline`, or `--surface structural` selects the frontend. Without the flag, the rc setting is used, then `readline` by default.

`RAL_INTERACTIVE_MODE=minimal` forces the canonical-stdin frontend regardless of the selected surface. An unknown value warns interactively and falls back to automatic probing. A structural surface that is unavailable in the build or cannot obtain raw terminal mode warns and falls back to readline.

### 16.6. Compatibility flags and environment

For tools that pass traditional shell flags blindly, ral accepts:

- `-e` — accepted with no effect;
- `-u` — accepted with no effect;
- `-i`, `-s`, and `-l` — with the meanings above.

ral seeds a stable dynamic environment at boot. Inherited values win; otherwise it supplies defaults for `HOME`, `USER`, `LOGNAME`, `PATH`, `SHELL`, `TERM`, and `LANG`. It increments `SHLVL` and supplies `OS_NAME`, `OS_ARCH`, and `OS_FAMILY`. Recognised terminal and multiplexer variables are retained when present.

`PWD` and `OLDPWD` are not exposed through `$ENV`. ral owns the current and previous directories as shell state so parallel work cannot race through the process-wide current directory. Each external child receives the correct `PWD`, `OLDPWD`, and actual launch directory.

`RAL_PATH` is a platform-separated list used to find modules and plugins. `RAL_TIMING`, when present, prints batch phase timings to stderr.

### 16.7. `ral-sh`: the POSIX bridge

ral is intentionally not POSIX-compatible. Registering `ral` itself as `$SHELL` can break `ssh host command`, `scp`, `rsync`, Git-over-SSH, editors, multiplexers, and other programs that expect `$SHELL -c` to parse POSIX syntax.

On Unix, install `ral-sh` as the login shell instead. It interprets no source; it only chooses a target and replaces itself:

- any short-option cluster containing `c` goes to `/bin/sh`, even `-lc`;
- `-l`, `-i`, or `--login` without `-c` goes to `ral`;
- no arguments with both stdin and stdout attached to terminals goes to `ral`;
- every other invocation goes to `/bin/sh`.

```text
/usr/local/bin/ral-sh -lc 'printf "%s\n" ok'  # /bin/sh
/usr/local/bin/ral-sh -l                       # ral
```

`ral-sh` forwards arguments without requiring UTF-8. If its own executable name begins with `-`, it preserves that login-shell convention as `-ral` or `-sh`. It looks for `ral` beside itself, falling back to `ral` on `PATH`. Failure to replace itself exits 127. Both `ral` and `ral-sh` refuse a setuid invocation.

### 16.8. Platform distinctions and limits

**Unix.** Terminal detection selects a bare REPL or stdin-script mode. Login-shell `argv[0]`, process-group job control, stop/resume behavior, and `ral-sh` are Unix facilities. Signals are translated into cancellation and conventional process statuses; the shell restores terminal ownership after foreground children.

**Windows.** A bare invocation defaults to the interactive mode because the CLI does not infer stdin-script mode from Windows terminal state; use `-s` for source redirected on stdin. ral enables virtual-terminal processing where available. Unix job-control and stopped-job behavior do not exist. Direct execution refuses `.bat` and `.cmd` images because their additional `cmd.exe` quoting pass has no safe general argument encoding; invoke `cmd.exe` explicitly only if that risk is acceptable. `ral-sh` is a Unix login-shell bridge and is not a usable Windows shell dispatcher.

On every host, external commands communicate through operating-system byte pipes. Value pipes are a ral runtime facility and do not silently become a cross-language wire format. Paths, executable lookup, path-list separators, default system paths, and sandbox enforcement otherwise follow the host rules described in their respective sections.

## 17. Formal language definition

This section collects the implemented core language in a compact notation. It
is descriptive, not a claim of soundness, completeness, progress, or
preservation. The parser, typechecker, annotated IR, and evaluator remain the
authorities when this presentation is ambiguous.

Metavariables used below are:

- `x` for an identifier, `w` for a word, `s` for a string, and `n` for an
  integer;
- `p` for a binding pattern, `v` for a value term, and `M`, `N` for
  computations;
- `A`, `B` for value types, `C`, `D` for computation types, and `ρ` for a row;
- `μ`, `ν` for pipe modes.

### 17.1. Concrete grammar

The lexer supplies words, quoted strings, interpolation segments, tags,
redirect tokens, newlines, and punctuation. A newline or `;` separates
statements. The shared stage parser requires every stage to end at a statement
boundary, `|`, `?`, `&`, a closing delimiter, or end of input; juxtaposed
same-line statements are not admitted.

The following EBNF omits lexical escape details and source-span bookkeeping.
`NL` denotes a newline and `sep` denotes `NL` or `;`.

```text
program       ::= sep* (statement sep+)* statement? sep*
statement     ::= binding | chain

binding       ::= "let" pattern "=" rhs-chain "&"?
rhs-chain     ::= pipeline (NL? "?" pipeline)*
chain         ::= bg-pipeline (NL? "?" bg-pipeline)*
bg-pipeline   ::= pipeline "&"?
pipeline      ::= stage ((NL* "|" NL*) stage)*

stage         ::= return
                | conditional
                | case
                | scope-form
                | command

return        ::= "return" atom?
conditional   ::= "if" atom atom
                  (NL* "elsif" atom atom)*
                  (NL* "else" atom)?
case          ::= "case" atom atom

scope-form    ::= "try" atom atom redirects
                | "guard" atom atom redirects
                | "within" atom atom redirects
                | "grant" atom atom redirects
                | "audit" atom redirects

command       ::= head (argument | redirect)*
head          ::= "^" bare-name | atom
argument      ::= atom | "..." atom

atom          ::= primary index*
primary       ::= word-value | block | collection
word-value    ::= lexical-word
                | quoted-string
                | interpolated-string
                | variable
                | expr-block
                | force
                | tag
index         ::= "[" word-value "]"        /* adjacent to its target */
force         ::= "!" primary
variable      ::= "$" identifier | "$(" identifier ")"
expr-block    ::= "$[" expression "]"
tag           ::= "`" identifier atom?

block         ::= "{" program "}"
                | "{" "|" pattern+ "|" program "}"

collection    ::= list | map
list          ::= "[" list-items? "]"
list-items    ::= list-item ("," list-item)* ","?
list-item     ::= atom | "..." atom
map           ::= "[:]"
                | "[" map-entry ("," map-entry)* ","? "]"
map-entry     ::= map-key ":" atom | "..." atom
map-key       ::= identifier | quoted-string | variable | tag-key

pattern       ::= "_" | identifier | list-pattern | map-pattern
list-pattern  ::= "[" pattern-list? "]"
pattern-list  ::= pattern ("," pattern)* ("," "..." identifier)?
                | "..." identifier
map-pattern   ::= "[" map-pattern-entry
                  ("," map-pattern-entry)* ","? "]"
map-pattern-entry ::= static-key ":" pattern ("=" atom)?
static-key    ::= identifier | quoted-string | tag-key

redirects     ::= redirect*
redirect      ::= fd? ">" word-value
                | fd? ">>" word-value
                | fd? ">~" word-value
                | fd? ">&" fd
                | fd? "<" word-value
                | fd? "<<" word-value
```

For `<<`, an explicit descriptor, if present, must be 0. `[]` is the empty
list and `[:]` the empty map. Otherwise the first
non-spread entry determines whether a bracketed collection is a list or a map.
A literal or pattern may use bare keys or tag keys, but may not mix the two
static key alphabets. A map literal may additionally use a dynamic `$name`
key; a pattern may not.

A plain identifier in head position enters ordinary name dispatch. A slash or
tilde path is a direct path head. `^name` requests external-name lookup. Other
atoms are value heads. With no arguments or redirects, a literal value head
remains a value rather than becoming a call.

The parser curries a multi-parameter block:

```text
{|p q| M}  =  {|p| {|q| M}}
```

Each pattern binds all its names simultaneously and may not bind one name more
than once. A list rest pattern is terminal. A map-pattern default is evaluated
only when its field is absent.

The `?` continuation admits at most one newline before `?` and none after it.
`|` admits newlines on either side. On a `let` right-hand side, a single final
`&` backgrounds the entire fallback chain; individual arms cannot be
backgrounded there.

Inside `$[...]`, expressions have the following precedence, from low to high:

```text
||
&&
== != < > <= >=
+ -
* / %
unary - and not
```

Binary operators associate left. `&&` and `||` short-circuit. Expression
operands are finite numeric literals, Booleans, variable or indexed-variable
references, forced atoms, and parenthesized expressions. The expression
grammar is deliberately smaller than the command grammar.

### 17.2. Core terms

Surface syntax elaborates into a call-by-push-value IR. Its essential value
and computation forms are:

```text
v ::= unit | true | false | n | f | s | bytes
    | [v1, ..., vn] | {l1 = v1, ..., ln = vn}
    | `l | `l v | thunk M | handle A

M ::= return v
    | force v
    | lambda p . M
    | M v
    | M to p . N
    | exec h [v1, ..., vn]
    | M1 | ... | Mn
    | M1 ? ... ? Mn
    | if v then M else N
    | case v of table
    | M1 ; ... ; Mn
    | letrec {xi = vi}
    | scope op
```

Command calls elaborate either to `exec`, which enters command dispatch, or to
application when the head resolves to a bound value. A trailing redirect on an
application or scope form becomes a redirect scope; redirects on `exec` remain
part of the atomic command launch.

Blocks elaborate to thunks. A parameterized block elaborates to a thunk whose
computation is a function. Adjacent recursive name bindings whose right-hand
sides are blocks or lambdas elaborate to one simultaneous `letrec` group.

### 17.3. Types and rows

Value and computation types are:

```text
A, B ::= Unit | Bool | Int | Float | String | Bytes
       | List A | Map A | Record ρ | Variant ρ
       | U C | Handle A | α

ρ ::= · | l : A, ρ | r

C, D ::= F[μin, μout] A | A -> C | β

μ ::= none | bytes | m
```

`Map A` is a homogeneous string-keyed map. `Record ρ` is a row-typed record;
`Variant ρ` is a tagged sum over the same row machinery. Bare record labels
and tag labels occupy distinct alphabets and do not unify. A nullary variant
has payload type `Unit`.

`U C` classifies a suspended computation. `F[μin, μout] A` classifies a
computation that may consume according to `μin`, may produce according to
`μout`, and returns an `A`. `none` is the ground mode for a value pipe;
`bytes` is the ground mode for a byte pipe. Mode variables are solved by
unification and any unconstrained mode is grounded to `none` before execution.

The checker is Hindley-Milner with value-, computation-, row-, and mode-level
unification variables. Let-bound names are generalized against the current
environment and instantiated freshly at use sites. Recursive groups are
inferred against monomorphic self-bindings, then generalized after those
self-bindings leave the environment. Runtime-created top-level bindings carry
their closed schemes into later runs.

The principal judgments are:

```text
Γ ⊢v v : A                         value inference
Γ ⊢c M : C                         computation inference
Γ ⊢p p : A ⇒ Γ'                    pattern binding
Γ ⊢ M ⇝ C ; K                      inference with constraints K
K ⊢ C1 ~ C2                        unification
```

A failed constraint records both the source span at which it was introduced
and a provenance reason, such as argument application, pipeline adjacency,
conditional branches, fallback branches, `try` arms, scope body, handler
shape, or `case` payload. Diagnostics render the structural mismatch together
with that provenance; provenance does not change the unification relation.

Representative rules are:

```text
Γ ⊢v v : A
---------------------------- Return
Γ ⊢c return v : F[none,none] A

Γ ⊢v v : U C
---------------------------- Force
Γ ⊢c force v : C

Γ, p:A ⊢c M : C
---------------------------- Lambda
Γ ⊢c lambda p.M : A -> C

Γ ⊢c M : A -> C    Γ ⊢v v : A
-------------------------------- App
Γ ⊢c M v : C
```

Pattern inference recursively constrains list elements to one element type,
record fields to their labelled types, and a list rest name to `List A`.
Binding a computation observes its result as described in §17.5 before
generalizing the bound value.

### 17.4. Pipe composition

Mode unification is equality:

```text
none  ~ none
bytes ~ bytes
m     ~ μ       binds m to μ
```

`none` and `bytes` do not unify. No implicit codec is inserted between a value
pipe and a byte pipe.

For adjacent stages `Mi | Mi+1`, inference first obtains their computation
types. After following any leading function arrows to the returned
computation, let `out(Mi)` and `in(Mi+1)` be their modes.

- If the producer has a value-pipe output and the consumer can accept another
  argument, composition is data-last application. The producer's value type
  must unify with that parameter type. A thunk producer whose body returns a
  value is forced exactly once for this transfer.
- Otherwise the producer's output mode must unify with the consumer's input
  mode. Ground `bytes`/`bytes` adjacency becomes an operating-system byte pipe;
  ground `none` adjacency is a value pipe.

The whole pipeline takes its input mode from the first stage, and its output
mode and value type from the final stage after any data-last application. The
annotation pass writes one ground `(input, output)` verdict and one value type
for each stage into the IR. Evaluation consumes these annotations; it does not
repeat type-directed mode inference.

Several compound forms join modes. Define the bytes-dominant join:

```text
bytes ⊔b μ     = bytes
none  ⊔b none  = none
μ     ⊔b ν     = their unifier when one exists, otherwise a fresh mode
```

For mutually exclusive arms, input uses the conditional union `⊔?`: preserve
the unified mode when the two modes unify, otherwise use a fresh mode that
surrounding context may pin. Output uses `⊔b`, because any selected arm that
writes bytes makes the compound byte-producing.

For a sequence, an input byte demand or byte output in any element is lifted
to the sequence. Its value type is the final element's. For `if` and `?`, arm
inputs fold with `⊔?` and outputs fold with `⊔b`.

### 17.5. Observation at a binding boundary

A byte-producing computation normally returns its explicit runtime value as
well as writing bytes. When a `let` binding captures such a computation, the
implemented observation function is:

```text
observe(Unit, bytes) = String
observe(A, μ)         = A        otherwise
```

At runtime, the first case captures standard output, removes one trailing
newline, decodes strict UTF-8, and binds the resulting `String`. If the
computation fails, bytes already written are flushed before the failure
propagates.

Branch result types are compared after observation under the compound form's
joined final output, not independently under each arm's local output. This is
why byte-writing `Unit` arms can agree with other arms whose captured result is
`String`. `audit` is the exception: its `value` field retains the body's raw
value type, while its byte fields contain captured bytes.

### 17.6. Branch and variant rules

For a two-armed conditional:

```text
Γ ⊢v v : Bool
Γ ⊢c M : F[i1,o1] A1    Γ ⊢c N : F[i2,o2] A2
observe(A1,o) ~ observe(A2,o) = A    o = o1 ⊔b o2
---------------------------------------------------------------- If
Γ ⊢c if v then M else N : F[i1 ⊔? i2,o] A
```

Additional `elsif` arms fold by the same rule. A conditional without `else`
is elaborated so that the selected body is evaluated for effects and the
whole form returns `Unit`.

A fallback chain uses the same arm merge as `if`; all successful arms must
therefore agree on one observed result type.

Variant construction introduces an open row:

```text
Γ ⊢v v : A
-------------------------------- Variant
Γ ⊢v `l v : Variant(`l : A, r)
```

For `case v table`, the scrutinee must have `Variant ρ`. The table must be a
record whose field at tag `l` has type `U (Al -> C)`, where `Al` is that tag's
payload type; every handler shares `C`. A literal table is closed and must
match the scrutinee's known tag set exactly. An opaque table remains open; a
missing handler is then an ordinary runtime failure naming the tag.

### 17.7. Scope signatures

Scope operators are typed in two steps. Each rule first produces a scope
signature:

```text
S = ([(run1,i1,o1), ..., (runn,in,on)], A)
run ::= always | on-failure
```

The signature records which arms can execute, their live pipe modes, and the
value produced by the scope. A single sealing operation then constructs
`F[i,o] A`:

- outputs fold with `⊔b`;
- inputs fold with `⊔b` when every arm is `always`;
- if any arm is `on-failure`, inputs fold with `⊔?`.

This run provenance is operational information, not an effect that capture may
erase: every scope arm runs against the scope's live streams. The scope-body,
handler-shape, and arm-result constraints also retain ordinary diagnostic
provenance and source spans.

Let `E` be the closed `try` error record
`{cmd:String, status:Int, message:String, line:Int, col:Int}`. The implemented
scope signatures are:

```text
body : U F[ib,ob] A
-------------------------------- Within / Grant
within opts body : ([(always,ib,ob)], A)
grant  caps body : ([(always,ib,ob)], A)

body    : U F[ib,ob] A
handler : U (E -> F[ih,oh] B)
observe(A,o) ~ observe(B,o) = R,  o = ob ⊔b oh
-------------------------------- Try
try body handler : ([(always,ib,ob), (on-failure,ih,oh)], R)

body    : U F[ib,ob] A
cleanup : U F[ic,oc] B
-------------------------------- Guard
guard body cleanup : ([(always,ib,ob), (always,ic,oc)], observe(A,ob))

body : U F[ib,ob] A
-------------------------------- Audit
audit body : ([(always,ib,ob)], AuditRecord A)
```

`within` additionally types known option fields and installs schemes for
literal handler entries while checking its body. `grant` types known
capability fields. Unknown, spread, or dynamic option keys remain subject to
runtime decoding.

### 17.8. Operational outcomes

Evaluation is big-step here only as a compact presentation. The implementation
uses an evaluator plus a tail-call trampoline. Write:

```text
<M, Σ, κ> ⇓ <R, Σ', κ'>

R ::= value v | error e | escape q
q ::= exit n | stopped job | tail call
```

`Σ` contains lexical bindings, session state, current directory, environment,
handlers, capabilities, status, and audit state. `κ` contains the current byte
pipe or value pipe endpoints and cancellation scope. Ordinary errors are
recoverable; escapes are a separate control channel.

The central rules are:

```text
<return v, Σ, κ> ⇓ <value v, Σ, κ>

<M, Σ, κ> ⇓ <value v, Σ1, κ1>
<N[v/p], Σ1, κ1> ⇓ R
-------------------------------- Bind
<M to p.N, Σ, κ> ⇓ R

<M, Σ, κ> ⇓ error e
-------------------------------- Propagate
<M ; N, Σ, κ> ⇓ error e
```

Sequences evaluate left to right, stop on the first error or escape, and
return the final value. `if` evaluates its Boolean condition and only the
selected branch, in a fresh lexical scope. `case` evaluates the scrutinee,
looks up its tag, and applies the handler to the payload or to `unit` for a
nullary tag.

For `M ? N`, a value from `M` wins; an ordinary error from `M` evaluates `N`;
an escape from `M` propagates. A longer chain repeats this rule and reports the
last error when all arms fail.

For `try body handler`, a body value is returned unchanged. A body error is
converted to `E` and passed to the handler. An escape bypasses the handler.
Standard output and standard error remain live; `try` does not capture them.

For `guard body cleanup`, cleanup runs after every body value or ordinary body
error. A cleanup value leaves the body outcome intact. An ordinary cleanup
error is diagnosed and discarded. A cleanup escape takes priority and
propagates.

`audit body` creates the structural audit root and evaluates `body` with that
trail active. Beneath the root, execution-call nodes are created only for
external or bundled commands and for builtin calls. User-function application,
control scopes, and iteration machinery are transparent: they create no
structural nodes of their own, while command and builtin activity reached
inside them remains visible. A builtin that implements iteration still has its
one builtin-call node; applying its callback does not add a function or
per-iteration wrapper node.

Capability observations are a separate audit-node kind rather than execution
calls. They are emitted only while an audit trail is active and at least one
enclosing capability layer has `audit: true`. In that case the capability
gates record their current allowed or denied filesystem and execution checks;
entering a capability layer may also record a flagged confused-deputy prefix.
Without both gates, capability checks add no audit nodes.

`fail r` requires an error record with a nonzero integer `status` and optional
`String` or `Bytes` `message`; it raises an ordinary error. `exit n` raises an
exit escape and is not caught by `?` or `try`.

Cancellation is polled at evaluator and process boundaries. A poll of a
cancelled scope raises an ordinary error carrying the strongest cause's fixed
message and status. The run boundary polls again after evaluation, so recovery
inside a final `try` cannot settle a cancelled run successfully. External
children are torn down and reaped before their cancelled computation returns.

### 17.9. Grounding and execution

Successful checking is followed by annotation. All remaining type, row,
computation, and mode substitutions are applied; quantified schemes are closed;
unconstrained pipe modes ground to value pipes; each pipeline stage receives a
ground wire description; and each `let` node records the final output mode of
its right-hand side.

Evaluation begins only from this annotated IR. A mode mismatch is therefore a
static error, not a request for the runtime to guess a codec. Runtime checks
remain for genuinely dynamic facts: missing record keys, bounds, opaque `case`
tables, dynamic option maps, operating-system failures, cancellation, and
external exit statuses.
