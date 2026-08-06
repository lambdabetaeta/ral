---
status: active
---

# A head has three identities, and a veto reads all three

**A command head is two spellings and a file: `shown`, `resolved`, and the
path they canonicalise to. Admission reads the spellings alone; every veto —
literal `deny`, `deny_dirs` — reads all three.** So a symlink cannot wear an
admitted name to reach a denied binary, and an allow cannot be inherited by a
link the grant never named. `core/src/runtime/command/identity.rs`,
`core/src/capability/exec.rs`.

## Context

The gate already split its identity sets asymmetrically — broad for vetoes,
narrow for admission
([[decisions/260731_one-walk-one-anchor|one-walk-one-anchor]]). The broad set
was the spellings plus their basenames, and no stage of head resolution ever
called `realpath(3)`: `walk_path` returns a `Path` head verbatim, and
`longest_dir_match` compared candidate strings lexically. Under

```ral
grant [exec: [bash: "deny", '/tmp/': "allow"]] { … }
```

a symlink `/tmp/b -> /bin/bash` therefore carried deny names `{/tmp/b, b}`,
met no `bash` key, and was admitted by the covering allow dir. The dir half of
the same gap was wider still: `longest_dir_match` read the *narrow* set for
both polarities, so `deny_dirs` could not see a link pointing out of the region
it denied.

macOS was already stricter than the gate here. Seatbelt matches
`(deny process-exec (regex #"/bash$"))` against the resolved vnode path, so it
refuses the symlink on its own; Linux renders no name rule at all
(`deny_basenames` has one consumer, `sandbox/macos.rs`), leaving the in-process
gate as the only enforcer of a name veto there.

## Decision

- **`CommandIdentity` carries a canonical form.** `canonicalise` runs one
  strict `realpath(3)` at `resolve`, through `Context::resolver` — the anchor
  the walk and `absolutize` already use — and stores `None` when no file is
  there, because nothing absent can owe a veto.
- **`deny_names_from` widens with the canonical path and its basename**, beside
  the basenames of both spellings.
- **`longest_dir_match` reads the set each polarity earns**: `allow_dirs`
  against the narrow spellings, `deny_dirs` against the broad set. Bare
  basenames fall to the existing `is_absolute` filter — a directory covers no
  bare name.
- **Admission is untouched.** The canonical form never enters `policy_names`,
  so an `allow` on `/bin/bash` does not reach a link to it.

## The limit, stated plainly

**A copy defeats a name veto and nothing can close that.** `cp /bin/bash
/tmp/b` under the grant above produces a different file, holding no trace of
the name that was denied; no resolution recovers it, and Seatbelt's regex sees
only `/tmp/b`. A name veto narrows an allow set. It is not a containment
boundary, and the boundary that holds is the confused-deputy property: an
exec-admitted directory must not be writable
(`core/src/capability/deputy.rs`). The gate pins this rather than leaving it to
be rediscovered —
`identity::tests::a_copy_under_a_new_name_is_a_different_file_and_is_admitted`.

## Consequences

- **One `realpath(3)` per external dispatch**, paid at `resolve` so the several
  vetting passes a command makes share it. Builtins, handlers, and env hits
  short-circuit before an identity is built, and the cost is nothing beside the
  spawn it precedes.
- **The deputy report does not cover the common case.** `deputy_prefixes`
  yields nothing unless *both* dimensions are restricted, deliberately — an
  unrestricted `fs` is not "everything writable". A grant that names `exec` and
  says nothing of `fs` gets no warning, so the copy above is unreported as well
  as unblocked.
- **Linux gains the most.** With no OS-level name rule behind it, the gate's
  new reach is the only thing that closed the symlink case there.
- **A relative head is canonicalised against `cwd_chain`**, the one "here" —
  taken against another cwd it would name another file, the failure
  [[decisions/260731_one-walk-one-anchor|one-walk-one-anchor]] describes.

## See also

[[internals/capability-enforcement|capability-enforcement]],
[[design/grant|grant]], [[map/core/capabilities|capabilities]].

Cite: `core/src/runtime/command/identity.rs` (`CommandIdentity`,
`canonicalise`, `deny_names_from`, `policy_names`),
`core/src/capability/exec.rs` (`ExecNames`, `layer_exec_verdict`,
`longest_dir_match`), `core/src/capability/deputy.rs` (`deputy_prefixes`),
`core/src/sandbox/macos.rs` (`emit_exec_rules`, `deny_basenames`).
