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
mirror changes into `ral.sublime-syntax`.  Test with:

    bat --list-languages | grep ral     # after bat cache --build
    bat doc/sgai-demo/05-powerhouse.ral # visual check

The grammar covers: comments, single- and double-quoted strings with
interpolation (`$ident`, `$(ident)`, `!{...}`, `$[...]`), let
bindings, lambda parameters, dereferences, spread, force, bypass,
tilde, arithmetic, redirection, pipes, chaining, and prelude
control names.
