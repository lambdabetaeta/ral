---
generated_at_commit: bb5bc1a6
status: active
---

# Unix sockets follow stated writes

**A pathname Unix socket bound outside the envelope is reachable exactly when
a stated fs write prefix covers its path, with an fs deny winning.** Binding
is likewise a write. A file rule such as ``write: [`absolute $SSH_AUTH_SOCK]``
admits that socket alone; a directory rule admits the sockets beneath it.

- **Only stated writes.** Unrestricted fs admits no outside socket. An
  exec-only, net-only or gpu-only grant must not be bypassable through tmux or
  an editor socket merely because it leaves fs unrestricted.
- **One resolver exception, macOS only.** `net` admits
  `/private/var/run/mDNSResponder`. Linux admits no resolver socket:
  nss-resolve returns `UNAVAIL`, and the stock
  `resolve [!UNAVAIL=return] dns` line continues to DNS over IP, reading
  `resolv.conf` and reaching the `127.0.0.53` stub in the shared netns. The
  glibc nscd client falls back likewise: measured with its socket denied,
  `getent hosts localhost` answers `::1`. Losing LLMNR/mDNS through
  nss-resolve does not justify another kind of shown object.
- **Old kernels retain a declared limit.** Below Landlock ABI 9 (Linux 7.1),
  every pathname socket a bind shows is reachable, even offline. No seccomp
  `AF_UNIX` row is added. The migration trigger and work are in
  [[internals/linux-envelope#When ABI 9 is the floor|the Linux envelope]].

## A socket admits the process behind it

**A server outside the envelope acts with the user's authority, not the
client's projection.** Admitting its socket may admit every access the layer
withheld:

- tmux voids fs, exec and net confinement; a tmux socket under a bound host
  `/tmp` is a full escape;
- docker.sock admits root;
- the user bus can `systemd-run --user` outside the projection;
- ssh-agent signs as the user.

This is the same deputy class as `launchservicesd`, which the macOS base
withholds ([[internals/seatbelt-profile|the Seatbelt profile]]).

A separate `sockets:` field was declined for ergonomics: admitting one socket
would make an exec-only grant restate reads under an fs operation. The existing
write spelling already expresses the object. Literal connect = write under
unrestricted fs was also declined: an exec restriction must hold now, while an
unrestricted fs still makes tmux and editor sockets visible.

## Concessions

**A stated write covering a deputy's socket admits that deputy.** Writing
`~/.docker` admits root and writing `~/.gnupg` admits signing. This is the same
concession as writing meets running: code or a service outside the projection
can exercise greater authority. The five restrictive bundled profiles write
only `cwd`, `gitdir` and `scratch`: `reasonable`, `minimal`, `confined`,
`edit-only` and `read-only` need writes for project changes where permitted
and disposable session work, so shared daemon directories supply no required
write authority. Session-owned scratch replaces shared temp specifically to
keep other programs' sockets out of that workspace. A foreign socket already under `cwd` or `gitdir` is still
admitted by the covering write; `dangerous` leaves fs unrestricted
([[design/grant|grant]] §Concessions).

- A server bound by one launch is unreachable from a sibling launch unless its
  socket lies under a stated write prefix. Measured on ABI 9, a sibling or
  parent-domain server returns `EACCES`; a same-domain server is reachable.
  `spawn { postgres -k ./s }` followed by `psql` normally has the write needed
  for binding and connecting alike.
- A writable directory holding a foreign socket lets a child unlink it and
  bind its own, intercepting the user's later clients: the write-side twin of
  writing meets running.
- exarch points `$TMPDIR` at `$EXARCH_SCRATCH/tmp` and creates it before use.
  The five restrictive bundled profiles grant no shared temp directory, since
  one can hold other programs' sockets. Hardcoded `/tmp` is per-command memory discarded on
  Linux and refused on macOS.
- Below ABI 9, every pathname socket a bind shows remains reachable.

## Rendering

**The Linux domain is entered after bwrap mounts; the macOS profile emits
write-prefix socket clauses.**

- ABI >= 9 handles `REFER | RESOLVE_UNIX` on every launch and adds `EXECUTE`
  where exec is restricted. The parent opens each write object and lowers
  directories onto the bind roots, sharing exec's lowering; socket-file writes
  get a rule on that opened inode. Fs denies are hidden by bwrap. The payload
  admits `REFER` on its own `/`, preserving cargo's cross-directory hard links.
  Every such domain handles fs rights and cannot mount; ral builds the ruleset
  before bwrap and enters it afterwards.
- macOS emits `network-outbound (subpath P)`, `network-bind` and
  `network-inbound (local unix-socket (subpath P))` for each rendered write
  name; fs denies emit the corresponding denies after the allows.
- macOS's base also admits Python multiprocessing's `/mp-` POSIX semaphore
  names and `/psm_` shared-memory names, including the leading slash.

See [[internals/linux-envelope|the Linux envelope]],
[[internals/seatbelt-profile|the Seatbelt profile]] and
[[map/core/capabilities|the capability map]].
