---
status: active
---

# A grant prefix is folded by the rule of the namespace that will match it

**Synod's grant names paths inside the Linux guest, so those prefixes are minted
through a POSIX kernel (`NormalizedPrefix::from_guest`, `lex::fold_dots_posix`)
rather than the host's — because the gate that matches them runs in the machine,
and on Windows the host's own kernel rebuilt `/work` as `\work` and denied the
agent the one folder it had just been given.** The freeze pipeline's promise was
never "one normal form"; it was "one normal form *per namespace*, on both sides
of a match". A single-namespace product cannot tell those apart. Synod on
Windows is the first thing in this repository that can, and it found the
difference the first time it ran outside a checkout.

## Context

[[design/capability-freeze|capability-freeze]] rests on a kernel: every path
that reaches a grant decision — the access-side `ResolvedPath` and the
grant-side `NormalizedPrefix` alike — is folded by `lex::fold_dots`, so
authorised-form and matched-form are one form and
[[design/grant|grant]] containment compares like-for-like. That argument is
sound and remains so. What it quietly assumes is that both sides are folded by
the *same* `fold_dots` — which is true whenever the process that freezes the
grant is also the process that enforces it.

Synod breaks that assumption structurally, and by design. Its authority is
`Capabilities` enforced by ral's gate *inside the guest*
([[design/two-enforcers|two-enforcers]] — the hardware boundary is the first
lock, the gate the second), while the grant is minted on the host, where the
user pointed at a folder. The paths therefore name the guest's namespace: the
folder at its mount point `/work` and the guest's disposable `/tmp`, never the
host path the user picked, which names nothing inside the machine. On macOS the
split was invisible, because a POSIX host folds a POSIX path to itself.

On Windows it is not invisible. `fold_dots` rebuilds its answer by pushing
`Path::components` into a `PathBuf`, and `Component::RootDir` renders as `\`
there — right for a host path (it also normalises `C:/x` to `C:\x`) and wrong
for a guest one. `/work` came back as `\work`: not a spelling variant but a
*relative* path in the namespace it claimed to name. The prefix crossed the wire
verbatim — `Frame::Attach` carries `cwd`/`home` unfolded, so those arrived
intact — and the gate in the guest, folding like the Linux it is, refused the
first read. The agent reported it exactly, off its own prompt:

```text
fs read: \work, \tmp        …but the mounted folder is /work
[R0001] Error: fs read denied by grant: /work
```

## Decision

**A second kernel and a second door, not a flag on the first.** `fold_dots_posix`
runs the identical law over `/`-separated components; `NormalizedPrefix::from_guest`
is its only caller. Two functions rather than one parameterised function because
they answer to two operating systems, and the caller always knows which one it
means — the same reason `starts_with_identity` takes `windows` as a parameter
instead of reading `cfg!(windows)`.

**`fold_dots` itself is left alone.** Its separator reconstruction is not a bug
to repair: for a host path on Windows it is the desired normalisation, and
`is_foreign_rooted` already depends on the `\tmp` it produces to classify a
POSIX path typed on a Windows host. The fault was never the kernel; it was
applying a host kernel to a guest path.

**This is the same line `vm-manager` had already drawn one field over.**
`MachineSpec::resolve` judges `workspace.guest_path` absolute with
`starts_with('/')` and says why `Path::is_absolute` would be wrong — nothing
without a drive letter is absolute on Windows, so the host's rule would refuse
every well-formed spec on that platform alone. Absoluteness had been moved into
the guest's namespace; folding had not.

**A prefix minted this way must not be *reduced* on the host.** `FsPolicy::meet`
re-mints its result through `PrefixSet::surface`, which folds with the host
kernel and would put `\work` straight back. Synod never reaches it: its trunk
runs with `fuel: 0`, so no sub-agent narrows its grant, and a nested `grant`
block inside the machine is reduced *there*, by the right kernel. A
guest-namespace policy that ever does need narrowing needs the meet to learn
which namespace it is in — recorded on the door, where the next caller will read
it.

## Why the tests were blind, which is the more useful half

`the_policy_admits_the_guest_namespace_and_denies_the_host_one` does the right
thing on paper: it runs the real grant through ral's real gate and asserts the
guest namespace is admitted and the host one refused. It passed on Windows
throughout, while the shipped product denied the first read.

It runs the gate **on the host**. So `sh.resolve("/work/letter.docx")` folds to
`\work\letter.docx` by the same broken rule as the prefix, and the two agree;
and `path_within` is separator-insensitive under Windows path identity anyway,
so it would have matched even if only one side had folded. Both halves were
wrong in the same direction, which is precisely what a host-side simulation of a
guest-side gate cannot see. The gate is not the thing under test in that
scenario — the *spelling* is, because the spelling is the whole of what crosses
the wire.

The new tests therefore assert on bytes: the fs read and write sets name
`/work` and `/tmp` exactly, on every host. A `#[cfg(unix)]` test pins the two
kernels to one law where a single machine can see both, so `fold_dots_posix`
cannot drift into a second, subtly different rule. Neither test can pass by
accident on either platform.

## Consequences

- The grant pipeline now has an explicit notion of *whose* namespace a prefix
  belongs to. It is carried by the constructor rather than by a field, which is
  enough for one guest namespace and would not be enough for two.
- Anything else the host mints for the guest is suspect by the same argument.
  Audited at the time: `cwd` and `home` cross unfolded through `Frame::Attach`
  and are correct; `MachineSpec::guest_path` is never joined on the host;
  `deny_paths` and `exec.dirs` are empty in synod's grant.
- A bug of this shape is invisible to a single-platform CI and to any test that
  simulates the guest's gate in the host's process. What caught it was running
  the installed product, and the agent reading its own prompt back.
