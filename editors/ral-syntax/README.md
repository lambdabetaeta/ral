# ral syntax grammar

Portable syntax definitions for the ral shell language.
Scope: `source.ral`.  File extension: `.ral`.

## Files

| File | Format | Consumers |
|------|--------|-----------|
| `ral.tmLanguage.json` | TextMate JSON | VS Code, Zed, TextMate, Sublime Text (via conversion), syntect |
| `ral.sublime-syntax` | Sublime Text / syntect YAML | bat, delta, presenterm (via bat cache), Sublime Text |

Both files define the same grammar.  The `.tmLanguage.json` is the
primary source; the `.sublime-syntax` is a hand-translated equivalent
for tools that require that format.

## Lexical contract

- Unit is `()`; bare `unit` is ordinary text.
- Single quotes are verbatim, with no doubled-quote escape. Adjacent
  quoted atoms are highlighted separately, not validated as a word.
- Bumped single strings (`#'a'b'#`, `##'a '# b'##`) support arbitrary
  fence lengths and multiline bodies in both formats. A closing quote
  consumes the opening fence's number of hashes; fewer hashes remain body
  text. A longer closing run leaves its excess hashes outside the string.
- Bumped strings take priority over comments. `#"x"#` is a comment,
  not an interpolating string. Mid-word hashes remain bare-word text.
- Numerals have an optional sign and either digits or a decimal point
  with digits on at least one side. Only point-bearing numerals admit an
  exponent: `.5`, `2.`, `-2.e+3`, but not `1e6`, `0x10`, or `1_000`.
  Entire words must match: `1abc` and `3.10.1` have no numeric prefix scope.
  In arithmetic, signs are operator scopes, including unary signs; exponent
  signs remain part of the number. Thus `$[1-2]` has two numeric operands.
- Interior colons, including `foo::bar`, remain word text; an entry colon
  splits before whitespace, a comma, a closing delimiter, or EOF.
- Double quotes recognize `\n`, `\r`, `\t`, `\e`, `\\`, `\0`, `\"`,
  `\$`, `\!`, `\~`, `\xNN`, `\u{H…}` (one to six hex digits), and
  escaped LF, CRLF, or CR. Escape value ranges remain runtime checks.
- Spread scopes only `...`, leaving any touching operand to the other
  lexical rules, including numbers, quoted strings, and collections.
- In double quotes, `!$name`, `!$(name)`, and `!$[expr]` scope the force
  sigil separately from their operands; `!name` remains literal text.
- Bare dereferences trim trailing hyphens (`$name-` scopes only `name`),
  while delimited dereferences retain them (`$(name-)` scopes `name-`).

These are lexical highlighters, not parsers or typecheckers. Numeric range
and adjacent-atom validity remain runtime checks. Commas are treated as
punctuation in all contexts; the runtime also permits mid-word commas
outside brackets. Neither definition validates classification-dependent
record/map spread restrictions.

## Installation

### VS Code

Create a minimal extension directory and symlink the grammar:

    mkdir -p ~/.vscode/extensions/ral-syntax
    cp ral.tmLanguage.json ~/.vscode/extensions/ral-syntax/
    cat > ~/.vscode/extensions/ral-syntax/package.json << 'EOF'
    {
      "name": "ral-syntax",
      "version": "0.1.0",
      "engines": { "vscode": "^1.50.0" },
      "contributes": {
        "languages": [{ "id": "ral", "extensions": [".ral"] }],
        "grammars": [{ "language": "ral", "scopeName": "source.ral",
                       "path": "./ral.tmLanguage.json" }]
      }
    }
    EOF

Restart VS Code.  `.ral` files get highlighting automatically.

### Sublime Text

Copy `ral.sublime-syntax` to your Packages/User directory:

    cp ral.sublime-syntax ~/Library/Application\ Support/Sublime\ Text/Packages/User/

### bat / delta

    mkdir -p "$(bat --config-dir)/syntaxes"
    cp ral.sublime-syntax "$(bat --config-dir)/syntaxes/"
    bat cache --build

Verify: `bat --list-languages | grep -i ral`

### presenterm

presenterm compiles its syntax set into a binary blob at build time
and cannot load external grammars at runtime.  Register the grammar
with bat (see above), then use `bat` via `+exec_replace` code blocks
in your slides to render highlighted ral.  See
`doc/sgai-demo/presenterm-config.yaml` for details.

### Zed

Place `ral.tmLanguage.json` in a Zed extension.  See
<https://zed.dev/docs/extensions/languages> for the layout.

### GitHub Linguist

Submit the `.tmLanguage.json` to the
[github-linguist](https://github.com/github-linguist/linguist)
repository following their contribution guide.

## Extending

Edit `ral.tmLanguage.json` first (it is the source of truth), then
mirror changes into `ral.sublime-syntax`. Run the shared behavioral scope
fixtures with Node.js/npm, Rust/Cargo, and a C toolchain installed:

    bash editors/ral-syntax/tests/run.sh # from the repository root

The runner installs pinned `vscode-textmate`/`vscode-oniguruma` and builds
syntect 5.3.0 with Oniguruma in temporary directories; it does not modify
workspace manifests or lockfiles. Cargo build artifacts are cached under
`/tmp/ral-syntax-scope-target` (override with `CARGO_TARGET_DIR`). Initial
runs require registry access. `tests/scopes.json` checks actual contiguous
scope spans, using the same cases for both engines, including multiline
state and fence backreferences. This verifies the Sublime YAML through
syntect, not a live Sublime Text installation or every downstream renderer.

For an additional visual check after `bat cache --build`:

    bat --list-languages | grep ral
    bat doc/sgai-demo/05-powerhouse.ral

The grammar covers: comments, verbatim single-quoted strings, double-quoted
strings with interpolation (`$ident`, `$(ident)`, `!{...}`, `$[...]`,
`!$ident`, `!$(ident)`, `!$[...]`), let
bindings, lambda parameters, dereferences, spread, force, bypass,
tilde, arithmetic, redirection, pipes, chaining, and prelude
control names.
