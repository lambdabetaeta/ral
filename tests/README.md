# Tests

Each crate has one integration binary, `<crate>/tests/integration/main.rs`,
whose top-level modules are the kinds below: `just test <kind>::` selects one.

| Kind | Question | Home |
|---|---|---|
| corpus | What does this program do, or what does `--check` say of it? | `tests/` |
| api | What does the engine hand a host: values, IR, schemes, observations, sessions? | `core`, `exarch` |
| cli | What does the `ral` process do: argv, env, rc, tty, signals, exit codes? | `ral` |
| assurance | Does the OS enforce the grant? Skips without confinement; fails under `RAL_REQUIRE_SANDBOX`. | `core`, `ral` |
| arch | Does the source text obey a structural law? | `lint/tests/arch` |
| types | Does the compiler refuse what the authority markers have not vouched for? | `core/tests/markers.rs` |
| ui | Do the renderer and the editor grammars behave? | `synod/ui/tests`, `editors/` |

A test belongs to the first kind that can say it. A `.ral` program with stdin,
stdout, status and stderr fragments is corpus; a value, a sink, a span or two
runs of one session is api; argv, env, rc or a tty is cli; kernel enforcement
is assurance; source text is arch.

## Corpus
- `reject/`: `ral --check` refuses. `accept/`, `../examples/`: it accepts.
- `run/{lang,builtins,unix}/`: `ral <name>.ral`, run from an empty directory
  with stdin `<name>.in` or null, prints `<name>.out`. `unix/` is Unix-only.

Directives are leading `# key:` lines; an unknown one-word key is an error.

    # status: N          exit status (reject: default 1 or 2; run: default 0)
    # contains: TEXT     on stderr, minus the program's own path (repeatable)
    # absent: TEXT       not on stderr (repeatable)
    # codes: T0050 ...   reject only: the diagnostic codes, in order
    # at: LINE:COL       reject only: where the first diagnostic points

## Commands

    just test [filter]   all runtime tests, e.g. `just test corpus::`
    just test-unit       unit tests alone
    just bless           rewrite run/**/*.out from current output
    just lint            formatting, clippy, docs and the arch rules
    just test-types      the markers; `just ui` and `just editors` the rest
