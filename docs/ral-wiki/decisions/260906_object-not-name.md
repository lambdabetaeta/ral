---
status: active
---

# Authorise the object, not the name; compose by stacking, not flattening

Prompted by an external review of the sandbox (`dev/docs/260906_sandbox_astra.md`,
six findings, all confirmed against `2cfeb108`). Four of the six were two
architectural faults; the other two were projection invariants. Nothing here
is a guard added on top — each fault is removed.

## Fault A: authority was a name, execution was an object

`check_fs_write(&rp)?; open(rp)` judged a leniently canonicalised *string*,
discarded the canonical form, and re-walked the original with `open(2)`. The
walks differ wherever a symlink does something `realpath` cannot follow: a
dangling link inside the writable directory canonicalised to *itself*, passed,
and `>>` then followed it to create a file outside the grant (reproduced). A
directory component swapped for a symlink between the two walks — by a
concurrent sandboxed child, which may write in the same directory — was the
same hole with a timer. And the write card's before-image was a *second* fs
door that never consulted read authority, so a write-only grant leaked the
old bytes into a script-readable `old_bytes`.

**Decision.** One door. `Shell::locate(rp, op)` walks the name from the root
through directory handles (`path/walk.rs`), never letting the kernel follow a
symlink: an intermediate component is opened `O_DIRECTORY|O_NOFOLLOW`; a
symlink met anywhere — leaf included — is read and spliced into the remaining
name, which is re-walked from the root (40 hops is a cycle). The path assembled
is canonical by construction. The verdict (`check_fs_exact`) is taken on that
object; the `Located` handed back performs open, stat, staging, rename and
unlink relative to the directory handle with `FollowSymlinks::No`. So a
dangling link is judged at its target, and a component swapped after the
judgment is never followed. `check_fs_write` is gone — every write is a
`locate`. The before-image is a read through the same `Located`, taken only
where `admits_fs_exact(Read)` says so; withheld, the write still lands and the
trail says `` `none ``.

`cap-primitives` (with `cap-fs-ext` for the `follow` switch) supplies the
`*at` primitives on Unix and Windows alike, so the walk is policy and not
platform code. `check_fs_op` remains for reads *by name* — predicates,
listings, module loading — where nothing is opened through the name.

## Fault B: composing by flattening a representation that is not closed

An exec layer is `(allow_dirs, deny_dirs, literals)`; a bare-name literal
means admission-or-restriction for whatever the name resolves to at check
time. `A ∧ B` with `git: [status]` on one side only means "A's restriction ∧
B's directory verdict on the resolved path" — expressible at the access, not
as a triple. `meet_literal_exec` chose to drop the one-sided key, and
`git push` was admitted where A alone refused it.

**Decision.** There is no `Capabilities::meet`. The `GrantStack` is the meet:
every verdict folds the layers' answers to the concrete access, where the
question *is* closed (`evaluate_exec`, `allow_region`, `deny_region`,
`permits_detach`). Every composer pushes layers — `--capabilities a,b`,
exarch's `--restrict` files, the deny layers for restrict and credential
files, the base a spawned child is narrowed to. The deputy lint folds the two
prefix sets it compares, which are closed under `meet_prefixes`. `Join` stays
for exarch's `--extend-base`, a union into one layer under its own "silence
lifts no veto" rule.

## Numbered redirects

`install_dup2` could only ever succeed on a descriptor the runtime already
owned — it refused `EBADF` — so `N>` for `N ≥ 3` had no legitimate case and
one illegitimate one: clobbering the Linux `Pin::Fd` that `/proc/self/fd/N`
resolves at `execve`. The lexer now admits fds 0–2 only; the whole `dup2`
path is deleted. fd 1/2 are `Sink`s, fd 0 a `Source`, and no other fd exists
in the language.

## Projection invariants

- Pins (`FsRules::pinned_dirs`) are derived from *rendered* deny names within
  rendered write names, so both an alias's chain and its target's are pinned;
  derived in surface space they missed the target's ancestors.
- The projection carries no allow beneath a deny (`PrefixSet::outside`):
  dead under deny-wins, and a Windows ACL orders explicit allows before
  inherited denies, so handing one over inverted the rule.

**Residual.** A Windows descendant with an inheritance-protected DACL does not
receive its directory's deny; characterising that needs a Windows host.
