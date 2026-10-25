---
status: active
generated_at_commit: da9576de
verified_at_commit: da9576de
verified_at_date: 2026-10-23
anchors: [OwnDir, UserFile, OwnError, Ungoverned, read_named, create_named, walk_dir_made, lock_strict, AccountHome, GrantNarrower, FreezeCtx, Pointer]
---

# Own files

**The files a program touches on its own account, outside any shell, are reached
only through `ral_core::own`: three addresses (`OwnDir`, `UserFile` and the
user-named functions), one authority (the crate-private write side of
`Ungoverned`), three authority-mint sites but no syscall sites, and one layout rule: the config home holds
only what the user writes.** This completes
[[decisions/261022_typed-sites|typed-sites]], which left 114 filesystem sites
outside `path/`, 93 of them raw `std::fs` on files the program owns. Each of those
is now an operation of `own`, and the filesystem sites outside `path/` fall from
114 to 21.

## Context

After the sites were typed, the commonest site was still a prose exemption: a
program opening, writing or deleting its own file. exarch's config, state,
tokens, keys, caches, records, logs and scratch, ral's rc, history and crash
log, synod's rootfs cache, and core's DACL ledger and frame dumps. No grant
judges any of them and none raises a card, since no model named the object. Three
things were wrong.

- **No controlled write path.** `Ungoverned`, the user's own authority with no
  grant over it, was read-only. Each site rebuilt the open, the atomic write, the
  owner-only mode and the cleanup, and differently: `state.json`, the model
  cache, `config.ral` and the rootfs marker were written in place, so a crash left
  a truncated file; the secret writer applied `0600` only when it created the
  file, so a `0644` `oauth.json` stayed `0644`; a second atomic writer used a
  fixed `.tmp` name and never synced its directory.
- **The temp fallback.** A program with no XDG base fell back to `$TMPDIR/<app>`,
  a predictable name in a shared directory that anyone can squat, and the session
  scratch held whole tool caches (cargo, npm, go: often gigabytes) on a RAM-backed
  `/tmp`.
- **A config home that collects what it should not.** A user who links
  `~/.config/exarch` into a dotfiles repository, with stow or by hand, would
  commit `keys.json` and ral's command history with it.

The 114 filesystem sites outside `path/` fell into classes by whose authority
names the file:
- 71 that the program keeps for itself;
- 10 that the user writes at a place the program chooses, and 7 that the user
  names (`MarkOfTheWeb` among them, which stays);
- 6 installed files, read-only, located from `current_exe`;
- 11 fixed host interfaces (the prelude bake and the NUL device go: the build
  scripts write the one, the walk's `open_discard` is the other);
- 9 on objects the walk had already reached.

So 93 become operations of `own` and 2 go elsewhere, and 19 stay: nine fixed
interfaces, the nine on objects the walk reached, and `MarkOfTheWeb`. The two jail
sites that the new bans of `create_dir` and `remove_dir` bring make 21.

## Decision

- **Three addresses, one per way a file is named.**

  | Address | Built from | Links | Offers |
  |---|---|---|---|
  | `OwnDir` | `OwnDir::xdg(app, kind)` only; children by one entry name | the root walk follows them; none is followed at a name beneath it | full management: `load`, `store`, `read`, `open`, `append`, `open_write`, `create_fresh`, `rename`, `remove`, `list`, `stat`, `dir`, `make_dir`, `within` |
  | `UserFile` | `xdg(app, kind, name)`, `home(name)`, `fixed(dir, name)`: compile-time names only | followed; each operation resolves the file afresh | `read`, `replace` (staged beside the file a link reaches), `seed` |
  | a user-named file | a `&Path` from the user, anchored on the cwd | followed | `read_named`, `open_named`, `kind_named`, `create_named` (a new file only) |

  An `OwnDir` is what the program keeps: state, tokens, logs, caches, records,
  scratches. A `UserFile` is a file the user writes at a place the program chooses
  (the rc and profiles, `config.ral`, house rules, exit hints, the `/etc` files),
  so a dotfile manager's link is respected and a replace lands beside the link's
  target, leaving the link and changing the repository behind it. The user-named
  functions serve a script, a seed, an export and synod's boot media, and admit one
  write, a file that did not exist.

  They take any path, so `clippy.toml` bans all four, and a model's string cannot
  reach them unseen. Each caller is a typed site, `Silent::<Variant>`, in its
  crate's `site.rs`, whose variant says why the name is the user's and not a
  model's: a CLI argument, a TUI command, or the install tree.

- **One authority.** `Ungoverned` gains `write` and `write_dir`, `pub(crate)`.
  `core/src/own.rs` is their only caller. Clippy's `disallowed_methods` bans
  all five authority-minting methods (`read`, `read_dir`, `region`, `write`,
  `write_dir`), requiring typed sites instead of a text-count allowlist.
  This module has three: `Silent::{OwnRead, OwnWrite, OwnWriteDir}`.
  A fourth authority marker, an `Owned` that keeps itself on
  descent, would restate in the type what the private fields of `OwnDir` and
  `UserFile` already guarantee.

- **No constructor from a path.** The roots and names are `&'static str` constants
  and an `XdgKind`; `OwnDir::within(path)` is positional: it walks `path`, takes
  the real position it reaches relative to the root's real position, and descends
  by the entry names of that difference from the root's held handle, so it returns
  only a descendant of the root it is called on, whatever the spelling of `path`;
  each name beneath a root is one `EntryName`, with no separator, no `.` or `..`, and no
  `:` on Windows. Types restrict *where*, not *who asks*: code that fed a model's
  string to `OwnDir::xdg("ral", Cache)?.make_dir(..)` would write into ral's cache
  with no grant, and only review stops that, as it stops any new site.

- **No syscall sites in `own.rs`.** Every syscall it needs is a primitive of
  `crate::path`'s walk, which is already where every object is opened:
  - `walk_dir_made`, which makes each absent name of a spelling owner-only and
    steps onto it, and refuses a name that a link's target spliced in, as
    `mkdir -p` fails a dangling link;
  - `Dir::target`, an entry as the object it is or, where nothing is there, the
    create target it would be, with `Located::linked_to` and `dangling` reporting
    where a link at the last name led;
  - `Located::{stage, open_data}` taking a `Visibility`, `Public`, `Private` or
    `Secret` (owner-only on every host, whatever the directory would hand down:
    `0600`, and on Windows a protected DACL naming the owner alone, its descriptor
    builder moved from exarch's secret writer into the walk's Windows host);
  - `Located::{remove, rename}`, which act by name under the held directory on
    Unix, as the new `Silent` variants `WalkRemove` and `WalkRename`, and by handle
    on Windows, adding no site. A tree is removed by descending with
    `locate(NoFollow)` and removing through the handle that located each entry, so
    a link is unlinked and never followed.

  The four user-named functions are banned, so none of them calls another: they
  share private helpers, and `own.rs` needs no site for them either.

- **The layout rule.** The config home holds only what the user writes; everything
  a program keeps lives in state, cache or data. This matches
  [[design/exarch-config-dir|exarch-config-dir]] ("hand-authored, trusted
  files"), and it is what makes a linked config folder safe to commit.

- **Roots are resolved once.** `OwnDir::xdg` walks the base once per process for
  each spelling and returns a clone of that handle after, so a caller asks for a
  root where it needs one and threads none through its API.

- **Errors say what to do.** `OwnError` carries `NoBase`, `NoCwd`, `Linked`,
  `Dangling`, `Outside`, `Io { act, at, to, error }` and `Json`, and each message
  names the place with `shown` and, where the user can act, the remedy: a link
  where the program keeps its own file, a dangling dotfile link, a link into a
  read-only store such as `/nix/store`, a run outside the state root, a
  `create_named` over an existing file. `From<OwnError>` exists for `String` and
  `io::Error`, so `?` works throughout.

- **The adjacent items.** `ral_core::host::{var, var_os, vars_os, temp_dir}` are
  the one reader of the host's environment, so every `std::env` ban points there
  and about thirty sites went; `LockExt::lock_strict` is the poison-keeping lock
  for state that restates something outside the process (a frame on a socket, a
  file's length); `invocation::exit_if_served` folds the one duplicated exit.
  `clippy.toml` gains bans on `std::fs::{create_dir, remove_dir, hard_link,
  soft_link}`, `DirBuilder::create`, the symlink makers, `std::env::{temp_dir,
  home_dir}` and the four user-named functions, and every `std::fs` ban now names
  `ral_core::own` as its replacement. `clipboard` reads no environment and
  depends on nothing of ral's: `Clipboard::new(remote)` takes whether the session
  is remote, and its caller reads `SSH_CONNECTION` through `host::var`.

## Consequences

- **The accepted decisions.**
  - *The temp root is dropped.* A scratch lives in
    `$XDG_CACHE_HOME/<app>/scratch/`, on disk and beside no one else's files, and
    ownership comes from the type, not from an `<app>-` name. The grant
    vocabulary gains a `scratch` place, written beside `` `tempdir . `` in five
    profiles (`minimal`, `confined`, `edit-only`, `read-only`, `reasonable`); a
    profile cannot spell it `xdg-cache`, since the session rebinds
    `XDG_CACHE_HOME`. The cost is that a crashed session's scratch is no longer
    cleared at reboot; the next launch's sweep clears it.
  - *`AccountHome` lives inside `host::xdg`.* Where no `HOME` is bound, the
    program's layout takes the account's home from the user database (the rule of
    `std::env::home_dir`), which covers `env -i`, cron and systemd units; past
    that, `NoBase` says what to set, and no temp directory stands in. The
    fallback sits in the one spelling of ral's own bases, so `app_dir`, `OwnDir`,
    `UserFile` and the credential denies see the same base, and a held root and a
    deny agree by construction. `host::home`, and so `~` in a script, is
    unchanged; `host::home_dot`, which `UserFile::home` reads, has the same
    fallback.
  - *`keys.json` and ral's history move to the state home*, with no shim:
    `$XDG_STATE_HOME/<app>/keys.json` and `$XDG_STATE_HOME/ral/history`.
  - *`Pointer.source` is stored as an `AgentId`*, resolved against the run's held
    `sessions/` directory. A run is movable, and every transcript read is
    relative to a handle. This changes a persisted format, and no compatibility
    is kept.
  - *`Ungoverned` has a crate-private write side*, as above.
- **`load` reports its errors (A1).** `OwnDir::load` returns
  `Result<Option<T>, OwnError>`: `Ok(None)` means only that nothing is at the
  name, and a link, an unreadable file or malformed JSON is an `Err`
  (`OwnError::Json`, whose kind is `InvalidData`, which `store` uses too for a
  value that fails to serialise). A caller that reads a cache or a remembered
  choice, and wants a corrupt file to read as none, writes `.ok().flatten()` and
  says so in one line: `state.json`, the model cache, the rootfs marker and the
  DACL stamp store.
  - **The rule for secrets.** Where a `Secret` store is read in order to be
    written back, a failed read aborts the write and returns its error: the OAuth
    token store and the keychain fallback map. Never write over a store that could
    not be read. Where such a store is read only to answer a question, a failure
    reads as none.
- **The scratch is carried as a spelling (A7′).** A grant place is a spelling, as
  `home` and `cwd` are, so the session's scratch is one too, and not a held
  directory: carrying an `OwnDir` in the shell would have forced a process-wide
  registry, since only wire data reaches `engine_root`, and left synod's guest with
  no scratch, so the spawn bases that say `` `scratch . `` would fail there.
  `SessionState.scratch` holds the spelling; `FreezeCtx::with_scratch` takes one;
  `engine_root` sets it from the host-authored root attach's `$EXARCH_SCRATCH`, on
  host and guest alike, and never from the overlay, so `within [env: …]` cannot
  redirect it; the wire seed carries it and decode restores it. `GrantNarrower`
  becomes `fn(&str, &FreezeCtx)`, so a spawn base sees the scratch. Plain ral sets
  none, and `scratch` is refused there with an error naming the place.
- **exarch's test doubles are test-only (A8).** `OwnDir` has no constructor from
  a path, so a production `pub fn` that opens own state at a path a test supplies
  cannot exist. The doubles (`AuditLog::for_test`, `Egress::for_test`,
  `Avatar::for_test`, `TestTrunk`, the scratch doubles) sit behind
  `#[cfg(any(test, feature = "test-util"))]` and are built on `OwnDir::for_test`
  and `OwnDir::temp_for_test`; exarch carries a self dev-dependency enabling
  `test-util`, as core does, guest-net and synod enable it in their
  dev-dependencies, `tempfile` is exarch's dev-dependency alone, and `fd-lock` is
  gone in favour of std's `File::lock` and `File::try_lock`. A fixture that makes
  nothing in its directory holds `OwnDir::temp_for_test`, the host's temp
  directory itself, with nothing made and nothing removed; one that makes
  something holds a `TestDir`, the guard `OwnDir::for_test` returns, which
  removes its directory when dropped.
- **A leftover file is neither read nor denied.** An old
  `$XDG_CONFIG_HOME/<app>/keys.json` and an old `~/.config/ral/history` are not
  read after the move, so a user without a credential manager re-enters a key
  once and history starts empty once, and `credential_files` names only the new
  location, so a confined agent that reads `` `xdg-config . `` could still read a
  stale `keys.json`. Delete it, or move it to the state home
  (`mv ~/.config/ral/history ~/.local/state/ral/history`).
- **What cannot be done cleanly.**
  - *Unix acts by name.* POSIX has no unlink, rename or publish by handle, so
    these act by name under the held directory. Anyone with write permission on
    that directory can swap the entry between the walk's step and the call; the
    call still never leaves the directory. Own directories are `0700`, and a
    `UserFile`'s target directory is the user's repository, under the user's
    authority. Windows acts by handle.
  - *A replace through a link is atomic in the target's directory.* The link
    itself is left alone; if the user retargets it meanwhile, the change lands
    where the link pointed when the walk passed it.
  - *Names still leave the handle* where a consumer needs one: reedline's history
    file, vm-manager's inflate target and boot media, the editor's argv,
    `$EXARCH_SCRATCH` and the grant over it. `OwnDir::path` is that boundary, and
    the consumer resolves the name again.
  - *`path` is a spelling.* `OwnDir::path` is the spelling the root was reached
    by (the app's directory plus entry names), not the walked real position.
    `within` compares no spelling: it is positional, so a run reached through a
    link above the root, or in another case on Windows, is found by where it is,
    and only a run that is not beneath the root is refused as `Outside`, with the
    advice to move it there. A link beneath the root is followed by where it
    lands: a link that lands inside the root resolves to the real directory it
    names, and a link that leaves the root is `Outside`.
  - *Windows pins held directories.* A held directory, its ancestors and every
    memoised root are held without delete sharing for as long as they live: the
    process's life, for a root.
  - *`MarkOfTheWeb` keeps its site.* It writes an NTFS stream, which the walk
    cannot address, since `EntryName` refuses `:`.
- **Platform behaviour not verified here.** The tests for these are cross-compiled
  only (`cargo xwin` for Windows, the `aarch64-apple-darwin` target for ral-core),
  never run on the platform, and a host that runs them is still wanted.
  - macOS tree removal through an `O_EVTONLY` directory handle;
  - Windows relative opens from an attributes-only handle;
  - `IGNORE_READONLY_ATTRIBUTE` on a `DELETE`-only handle;
  - Windows lock semantics: `File::lock` as the rotation lock of the audit ledger
    on every host, and the order of closing a lock file before unlinking it.
- **Cheaper and safer in passing.** Every JSON file a program keeps is replaced
  whole and atomically, so a crash leaves the old file or the new; a secret is
  owner-only whatever was there; the audit-ledger rotation is serialised between
  processes on Windows too; and a scratch lock taken by another exarch's sweep in
  the instant after it is made yields a new name, and after three tries an error,
  never a panic. `--resume <path>` accepts any run under the app's state root,
  reopened from the held root with `OwnDir::within`, and refuses one elsewhere
  with a message saying to move it there.

### Site counts

Filesystem sites outside `path/` fall from 114 to 21: the 11 fixed host interfaces
(the two new jail sites among them), the 9 on objects the walk reached, and
`MarkOfTheWeb`. The `site!` totals:

| Crate | Before | After |
|---|---|---|
| core, outside `path/` | 96 | 59 |
| `core/src/path/` | 86 | 88 |
| exarch | 83 | 17 |
| ral | 16 | 6 |
| synod | 15 | 4 |
| guest-net | 2 | 2 |
| clipboard | 1 | 0 |
| **six crates** | **299** | **176** |
| vm-manager | 51 | 54 |
| **workspace** | **350** | **230** |

`path/` gains two, `WalkRemove` and `WalkRename`; vm-manager gains three for the
new bans (its broker, its Windows backend and its macOS backend) and was
otherwise left alone. The seven callers of the user-named functions are sites
too: exarch has four, ral one and synod two.

## Rejected

- **A fourth authority marker.** See the decision: the private fields already say
  it.
- **Keeping the temp root.** `OwnDir::temp(app)` would admit and list only
  `<app>-…` names, and `make_dir_new` and a prefix check in the scratch sweep would
  stay, to keep ownership by naming in a shared directory, with tool caches on a
  RAM-backed `/tmp`.
- **Refusing a link at `config.ral`.** Withdrawn: a per-file dotfile link could no
  longer be rewritten by synod's accounts screen. A `UserFile` follows it.
- **A shim for the old locations of `keys.json` and the history.** None is wanted.
- **A general `process::exit` helper.** Each of the five exits rests on a
  different fact; only the one duplicate, a served re-exec role having run, folds
  into `exit_if_served`.

## Where

`core/src/own.rs` (`OwnDir`, `UserFile`, the named functions, `OwnError`);
`core/src/path/walk.rs` and its host modules (`walk_dir_made`, `Dir::target`,
`Visibility`, `remove`, `rename`); `core/src/capability/fs.rs` (`Ungoverned`);
`core/src/host.rs` (`var`, `var_os`, `vars_os`, `temp_dir`, and the account-home
fallback in `xdg`); `core/src/sync.rs` (`lock_strict`); `core/src/guard/freeze.rs`
(`FreezeCtx`, the `scratch` place) and `core/src/guard/grant.rs`
(`GrantNarrower`); `exarch/src/app.rs` (`App::own`, `Scratch`, `RunLock`);
`clippy.toml` (the bans and their replacements); `core/src/site.rs`
(the authority-mint variants); `core/tests/sites.rs` (typed-site enforcement). The companion record is
[[decisions/261022_typed-sites|typed-sites]]; how the files are laid out is
[[design/exarch-config-dir|exarch-config-dir]], and the session record's pointers
are in [[internals/session-record|session-record]].
