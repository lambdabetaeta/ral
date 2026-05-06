# nvim tree-sitter integration

Syntax highlighting for ral in nvim is provided by a tree-sitter parser in
`editors/tree-sitter-ral/`. The compiled parser lives outside the repo at
`~/.local/share/nvim/site/parser/ral.so`. The highlight queries are in
`~/.config/nvim/queries/ral/highlights.scm` (also tracked in
`dotfiles/.config/nvim/queries/ral/highlights.scm`).

## Prerequisites

- `tree-sitter` CLI: `brew install tree-sitter`
- `node` (used by tree-sitter generate): via nvm or `brew install node`

## First install

```sh
cd ~/projects/ral-private/editors/tree-sitter-ral
tree-sitter generate    # produces src/parser.c from grammar.js
```

Then in nvim:
```
:TSInstall ral
```

## After changing grammar.js

```sh
cd ~/projects/ral-private/editors/tree-sitter-ral
tree-sitter generate
```

Then in nvim: `:TSInstall ral`

## After changing highlights.scm

Copy the updated file to the live location:

```sh
cp ~/projects/ral-private/editors/tree-sitter-ral/queries/highlights.scm \
   ~/dotfiles/.config/nvim/queries/ral/highlights.scm
# if ~/.config/nvim is not symlinked to dotfiles, copy there too
```

## How it works

- `*.ral` files get filetype `ral` via the autocmd in `init.vim`.
- `after/plugin/ral-treesitter.lua` injects the ral parser config into
  nvim-treesitter via a `package.preload` hook. The hook re-fires on every
  `:TSInstall` / `:TSUpdate` run because nvim-treesitter explicitly clears
  `package.loaded['nvim-treesitter.parsers']` before each operation.
- `:TSInstall ral` compiles `src/parser.c` and installs `ral.so` into
  `~/.local/share/nvim/site/parser/`.
- Highlighting queries are read from `~/.config/nvim/queries/ral/`.
