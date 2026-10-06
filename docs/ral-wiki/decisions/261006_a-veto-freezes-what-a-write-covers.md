---
status: active
generated_at_commit: b49b3823
verified_at_commit: b6bb51b5
anchors: [WriteReach, write_reach, emit_exec_rules, carries_veto, Binds, vetoes_walk]
---

# A veto freezes what a write covers

**An admitted directory is frozen against writes unless the write region names
that tree, or one that holds it: writes named at or below an admit at or above
this one keep it writable on purpose; a broad prefix that no admit holds does
not.** Under a restricted `fs` the freeze fires whenever exec is restricted,
veto or none: the hole needs an admitted set, not a veto. The kernel renders it
on macOS and Linux alike.

## Context

The veto is policy about what runs, and, in one case, the integrity of the
host's binaries. Containment is not its stake: `fs` and `net` confine the
child whichever binary it runs, and an admitted interpreter already runs
arbitrary code in process. The case is authoring: a child that can write into
an admitted directory can put a binary there that the veto does not name, and
run it, or leave a trojan in a directory the user's own `$PATH` resolves
unconfined tomorrow. Copying the denied binary under a new name is the same
hole; the child need not even copy, an admitted interpreter writes a fresh
binary byte by byte.

macOS closed this under `fs: Unrestricted` alone, freezing the admitted set
when a veto was in force
([[internals/seatbelt-profile|seatbelt-profile]]). Under a restricted `fs` it
stood aside on the reading that an overlap of write and admit is a stance
someone took. That is true where the grant names the tree, and false where a
broad prefix covers an admitted directory the author never mentioned: `write:
~` over the admit `~/.cargo/bin`, or any write region over a system directory
the projection admits.

The obvious fix, freezing every admitted directory a write region covers,
breaks the default shape. exarch's bases write `cwd:` and admit `cwd:/` on
purpose, so that an agent writes `./scripts/` and then runs
`./scripts/build.sh` ([[design/grant|grant]], the exec-admission concession).

## Decision

- **Classify each admitted directory `A` against the write prefixes**, all as
  rendered names, by one function, `FsRules::write_reach`, on the plain prefix
  test:
  - *Trusted*: some write prefix `W` that reaches `A` (`W` covers `A`, or lies
    inside it) itself lies within, or is, an admitted directory `T` that
    contains, or is, `A`. The writes were named at or below an admit at or
    above `A`, so authoring binaries there is the grant's intent. No freeze.
  - *Covered*: some write prefix covers `A` and none qualifies as above.
    Frozen.
  - *Apart*: no write prefix covers `A` and none qualifies. Nothing to
    freeze.

  So `write: cwd` with `cwd/` admitted trusts `cwd/` and, through it,
  `cwd/node_modules/.bin`; `write: ~` over a lone `~/.cargo/bin` covers it,
  since no admit holds both. Trust does not leak sideways: with `write: ~` and
  `write: ~/.cargo/registry` and admits `~/.cargo/` and `~/.cargo/bin/`, the
  carve-out lies in `~/.cargo/` and trusts it, but does not reach
  `~/.cargo/bin/`, and `~` lies within no admit, so `~/.cargo/bin/` is covered.
  Rendered names make the two firmlink spellings of a path agree.
- **macOS emits `(deny file-write* (subpath <admit>))` for each covered
  admit**, in the section after the write allows, so last-match-wins puts it on
  top. The loader base's directories are classified with the grant's. Under
  `fs: Unrestricted` nothing changes: the whole admitted set, files included,
  is frozen when a veto is in force, and not otherwise.
- **A bare-name veto holds only where the admit set is frozen.** Under a
  trusted admit the child may copy the denied binary into the workspace under
  a new name and run it with `sh -c`: the kernel admits the subpath, the
  `/name$` regex does not match the new name, and the gate never sees
  grandchildren. Extending a bare-name deny to `file-read*` by regex would
  deny reading any same-named file. So under a trusted admit a bare-name veto is
  advisory.

The cost model reads well: who wants writes under an admitted directory names
the tree and gets it; who names a broad prefix keeps broad writes, but not the
silent ability to author binaries inside the admitted directories within it.

## Consequences

- **Linux renders it in the envelope.** `Binds::open` lays a read-only bind
  over each covered admit, by the handle it opened, in a layer of its own
  (`Frozen`) after the writable prefix that covers it and before `/proc`;
  under `fs: Unrestricted` with a veto, over the whole admitted set, files
  included, after `--dev-bind / /`
  ([[decisions/261006_the-envelope-mounts-by-handle|the-envelope-mounts-by-handle]]).
  The Landlock exec layer carries a veto into every covered and apart admit by
  subtraction, and not into a trusted one, and both read one classification
  (`landlock::write_reach`, over `FsRules::write_reach`): the hierarchies a
  veto walks are exactly those the envelope keeps unwritable. A trusted
  admit's bare-name veto stays advisory on both platforms.
- **Only directories are classified.** An admitted file under a covering prefix
  stays writable, as it did; files are frozen only under `fs: Unrestricted`
  with a veto.
- **Option not taken: freeze whenever an admit set exists, under any `fs`
  shape.** It is the more honest reading, and breaks exec-only grants that
  write user-owned system directories, `brew install` among them. It waits for
  someone to ask.
