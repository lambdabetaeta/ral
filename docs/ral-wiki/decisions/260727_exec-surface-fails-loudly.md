---
status: active
---

# Two exec spellings that failed quietly now fail at load

Follow-up to
[[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]],
which fixed the *internal* exec type. The surface encoding kept two warts, both
of the same kind: a plausible spelling that the decoder accepted and then gave
the author something other than what it looks like. Neither is a lattice
question — the value vocabulary (two-valued for directories, three-valued for
literal commands, `argv[0]` depth only) stands.

## A path-shaped literal key that is a directory is an error

The trailing slash is the whole of what separates a subpath key from a literal
one. So `exec: ['/usr/bin': 'allow']` decoded to a literal grant on a *binary*
at the path `/usr/bin` — a file that cannot exist. It matched nothing, and
deny-by-default then refused every command the author meant to admit, surfacing
much later as a bare "denied by active grant" with no hint that a slash was the
cause. Failing closed, but unreadably.

`freeze_exec_map` now stats a path-shaped literal after freezing it and errors
if it is a directory, hinting `'/usr/bin/'`. The freeze pass already consults
the environment — `path:` reads `$PATH`, sigils resolve against the `FreezeCtx`
— so asking the filesystem there is in keeping with what freezing already is.

The alternative was an explicit `dir:` sigil alongside `path:`/`xdg:`/`cwd:`,
making the kind spelled rather than shape-inferred. Rejected as the larger
change to a widely understood convention: the slash reads correctly, it just
needed to stop being silent when omitted.

## An empty subcommand list is an error, not a third spelling of `'allow'`

The decoder mapped `[]` to `ExecPolicy::Allow`. But
`Subcommands(s₁).meet(Subcommands(s₂))` is set intersection and legitimately
produces `Subcommands(∅)`, which `check_exec_args` enforces as *deny every
invocation*. The same object meant ⊥ inside the lattice and ⊤ at the surface.

It was also anti-monotone at the boundary: deleting entries from a subcommand
list shrinks authority right up to the last deletion, which jumps to full
authority — the same inversion the 260602 dir-meet fix closed elsewhere. Given
the decoder's otherwise strict brand (rejects `true`, rejects capitalised
`Allow`, errors on unknown keys), the empty list is now an error naming both
real spellings.

`decode_exec_grant_empty_list_means_allow` pinned the old behaviour and is
replaced by its inverse; the `[]`-as-allow spelling is gone from SPEC.md,
TUTORIAL.md, both READMEs, and every test and demo script that used it.
