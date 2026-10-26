# Editor support

This directory contains editor integration files for ral.

| Directory / file | Contents |
|---|---|
| `ral-syntax/` | Portable syntax definitions (TextMate JSON, Sublime syntax) for VS Code, Zed, Sublime Text, bat, delta, presenterm |
| `tree-sitter-ral/` | Tree-sitter grammar and highlight queries for nvim (and other tree-sitter consumers); parser sources/libraries are generated, not tracked |
| `nvim.md` | Installation and update instructions for nvim |

See `ral-syntax/README.md` for per-editor installation instructions (VS Code, Sublime Text, bat, Zed, …).
See `nvim.md` for nvim tree-sitter setup.

The language authority is `docs/SPEC.md` and `core/src/syntax/`, not these
highlighting approximations. The grammar READMEs name their limits: tree-sitter
supports bumped single-string fences through depth 3; the regex bundles support
arbitrary matching fences. Neither validates numeric ranges or
classification-dependent record/map spread restrictions.

Verification:

```sh
(cd editors/tree-sitter-ral && tree-sitter generate && tree-sitter test)
bash editors/ral-syntax/tests/run.sh
```

The scope fixtures execute TextMate/Oniguruma and Sublime YAML through
syntect/Oniguruma; they do not stand in for a live Sublime Text test.
