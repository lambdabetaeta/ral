# nvim tree-sitter integration

Syntax highlighting for ral in nvim comes from the tree-sitter grammar in
`editors/tree-sitter-ral/`. Only `grammar.js` and `queries/highlights.scm` are
tracked; the parser C source and the compiled library are generated from them,
and both are installed outside the repo — the library at
`~/.local/share/nvim/site/parser/ral.so`, the queries at
`~/.config/nvim/queries/ral/highlights.scm`.

## Prerequisites

- the `tree-sitter` CLI — `brew install tree-sitter`, or
  `cargo install tree-sitter-cli`
- a C compiler reachable as `cc`

## Install, and re-install after a change

`scripts/install-treesitter.ral` is the whole procedure: it runs
`tree-sitter generate` over `grammar.js`, compiles the resulting
`src/parser.c` into `ral.so`, and copies `highlights.scm` into place.

```sh
scripts/install-treesitter.ral
```

Run it again after editing either `grammar.js` or `queries/highlights.scm`.
nvim loads the library at startup, so a restart is the only other step.

If you already install parsers through nvim-treesitter, `:TSInstall ral` is the
alternative route. It compiles `src/parser.c` itself, so `tree-sitter generate`
must have run first, and the grammar has to be registered in your
nvim-treesitter config with `src/parser.c` as its source file.

## What nvim needs on its side

Neither route configures the editor. Two things have to hold, and both belong in
your own nvim config rather than in this repo: `*.ral` files must resolve to
filetype `ral`, and the `ral` language must be registered against that filetype
so nvim finds the installed library and reads queries from
`~/.config/nvim/queries/ral/`.
