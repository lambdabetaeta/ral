---
status: active
generated_at_commit: da9576de
verified_at_commit: da9576de
verified_at_date: 2026-10-23
anchors: [site!, Site, Surface, Silent, spelled_parent]
---

# Typed sites

**A banned call is exempt only as a typed site: `site!` wraps exactly one call,
and its first argument is a variant of a crate-private enum that names the place
and says why it is exempt.** The enums are the manifest. The exemption is
`#[expect]`, so a site that wraps no banned call is an error, and checks in
`core/tests/sites.rs` state what clippy cannot: that no other exemption exists,
that no lint group silences the lint, and that every member is bound by it. This replaces the tagged `#[allow]` of
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]
(Enforcement); how the enforcement reads now is told in
[[map/exarch/io-surface|io-surface]].

## Context

`clippy.toml` bans the constructors and the raw wrappers under one lint,
`clippy::disallowed_methods`, and a legitimate call opted out with a
hand-written `#[allow(clippy::disallowed_methods, reason = "…")]`. Three things
were wrong with that.

- **One lint, so exemptions were prose.** An allow names the lint, not the ban,
  so an allow written for a spelling conversion silenced an `fs::read` as
  readily. The reason opened with a tag (`[surface:<slug>]`, `[silent:<slug>]`,
  `[test]`, and the untagged `native:`, `path-form:`, `host-env:`, `reaper:`,
  `REASONED-SILENT:`), and only a scanner reading text held the tags to a
  manifest.
- **Function scope masked.** Most allows sat on whole functions, so one
  exemption silenced every banned call in its body, including the ones added
  later.
- **Nothing noticed a stale one.** An `#[allow]` over code that no longer
  contains a banned call is silent. Seventeen were found when the sites were
  converted.

## Decision

- **A site wraps one call.** `site!(Silent::ArgvBeforeMain,
  std::fs::read("/proc/self/cmdline"))`; for a method,
  `site!(Silent::WhichStat, &candidate => is_file())`. The receiver and every
  argument are evaluated *outside* the exemption, so a banned call nested in an
  argument still fires. The macro's result chains (`site!(…).unwrap()`,
  `site!(…)?`), a chain of banned calls is split into one site each, and `?` and
  `.await` stay outside the macro.
- **The first argument is a variant, and the variant is the manifest.** It must
  implement the crate's `Site` marker trait. `Surface` lists the sites that
  raise a card, `Silent` those that raise none; a crate defines only the enums
  it uses. A variant's doc says what the place is and why it is exempt (and, for
  `Silent`, why it raises no card). Several calls that serve one purpose share a
  variant. The enums are `pub(crate)`, or `pub(in crate::path)` in
  `core/src/path/site.rs`, carry no `ALL` constant and no registry, and derive
  `Clone, Copy` alone, so `dead_code` reports a variant that no site uses; a
  variant used only under a `cfg` carries that `cfg`.
- **The exemption is `#[expect]`.** The macro emits
  `#[expect(clippy::disallowed_methods, …)]` on the one call, so a site that no
  longer wraps a banned call is `unfulfilled_lint_expectations`, and a stale
  exemption cannot outlive its call. Rustc alone does not run the lint, so only
  clippy reports a stale site.
- **The macro file is `include!`d, not shared.** `lint/site.rs` at the repo root
  holds the `Site` trait and the macros; each crate that has sites mounts it
  from a module named `site` at its crate root (`src/site.rs`, whose first item
  is the `include!`, followed by that crate's enums), and the macro addresses
  `$crate::site::…`. A shared crate would export the macro, and a lint
  expectation placed by a macro defined in another crate is never reported as
  unfulfilled, so the stale check would be lost. The macro is crate-local, with
  no `#[macro_export]`.
- **Test, example and build code is not what the bans protect, and carries one
  blanket per unit.** Each file directly under `<crate>/tests/` opens with an
  inner `#![allow(clippy::disallowed_methods, reason = "test code")]`; a
  `#[cfg(test)]` module carries the outer allow beside its `#[cfg]`, and nothing
  inside it carries one; an example and a `build.rs` carry the inner allow with
  their own reason. `ral-daemon` and `ral-initramfs` keep their crate-root
  allow: each is a guest init program, whose whole job is the syscalls the bans
  route, and links no ral shell.
- **A wrong conversion is fixed, not exempted.** Where an allow existed to
  paper over a lossy or by-spelling path operation, the code changed instead:
  `path::utf8` is the one strict conversion for a path that must travel as
  text, `path::shown` the one lossy conversion, for display only,
  `path::spelled_parent` the by-spelling parent for a program's own layout, and
  bytes stand in for a conversion where only a comparison was wanted.
- **`core/tests/sites.rs` states what clippy cannot.** Its first check scans the
  Rust sources of every workspace member but `vendor/`, comments stripped, and
  every occurrence of `clippy::disallowed_methods` must be one of four:
  - (a) in `lint/site.rs`;
  - (b) an inner `#![allow(clippy::disallowed_methods, reason = "…")]` among
    the leading attributes (only other inner attributes precede it) of an
    integration-test crate root, an example, a `build.rs`, or
    `ral-daemon/src/lib.rs` or `ral-initramfs/src/lib.rs`;
  - (c) an outer `#[allow(clippy::disallowed_methods, reason = "test code")]`
    on a `mod` item whose own attributes include a `#[cfg(…)]` whose predicate
    implies `test`: exactly `test`, an `all(…)` with a `test` conjunct, or
    exactly `any(test, feature = "test-util")`. Any other `any(…)` holds in a
    production build, and `any(test, unix)` would exempt production code on Unix;
  - (d) the one third-party-codegen exception, below.

  A failure names `file:line` and the rule it breaks.

  Two further checks close the other ways round the lint's name.
  - **No group silences the lint.** `disallowed_methods` is a `style` lint, and
    `clippy::all` holds `style`, so an `allow`, `expect` or `warn` of either group
    would silence every ban without naming it. A test over the same sources
    flags, with its `file:line`, every attribute that names either group and is not a `deny` or a
    `forbid`.
  - **Every member is bound.** The bans and their `deny` are the workspace's
    lints, which a member takes through `[lints]` with `workspace = true` in its
    `Cargo.toml`. A test reads the manifest of every workspace member but
    `vendor/`'s, which opts out by design, and fails one that lacks the table.

  The two pinned caller sets stay: the production files that read through
  `capability::Ungoverned` (`ungoverned_gains_no_new_caller`), and the files
  outside `path/` that call the string containment helpers.

## Consequences

- **The macro has limits, each with its workaround.** It binds the receiver and
  each argument once, outside the exemption, and every limit follows from that:
  - a `&mut` that is used again is moved into the macro: write `&mut *v`;
  - a by-value receiver that is not `Copy` is moved: write `&x => m()`;
  - a closure argument loses its expected signature: annotate its parameters,
    `|s: &str| …`;
  - an argument whose type only the callee's parameter fixes cannot be inferred
    from the macro's binding: bind it first, with an annotated type;
  - two-phase borrows are lost, so `site!(V, &mut v => push(v.len()))` fails:
    bind `v.len()` first;
  - the result cannot borrow from a temporary created in the arguments: bind
    the temporary first;
  - a banned function used as a value (`.and_then(Path::parent)`) becomes a
    closure with a site inside, or a helper.
- **A crate whose lib and bin targets both have sites mounts the module twice.**
  The module is crate-local, so the library's `site` cannot serve the binary:
  synod's bin mounts its own with `#[cfg(windows)] #[path = "shell/site.rs"] mod
  site;` in `main.rs`, and vm-manager's broker binary has a file of its own,
  `src/bin/synod-machine-broker/site.rs`.
- **One third-party-codegen exception, pinned.** `tauri::generate_context!()` in
  `synod/src/main.rs` expands to code that exits the process, and prints, when
  building the context panics, and `site!` wraps calls, not macro invocations.
  The statement carries a hand-written
  `#[expect(clippy::disallowed_methods, clippy::disallowed_macros, …)]`, which
  the scan permits as rule (d), by file and by the `generate_context!` it
  precedes; being an `#[expect]`, it is still reported if the expansion stops
  needing it.
- **A program's own files take no site.** Of the filesystem sites outside `path/`,
  93 were raw `std::fs` on files the program keeps, the user writes at its places,
  or the user names. They became operations of `ral_core::own`, whose every
  syscall is a walk primitive, so the module holds no `site!`, and the filesystem
  sites outside `path/` fell from 114 to 21. The four functions that take a name
  the user gave are banned, and each caller is a typed site that says why the name
  is the user's ([[decisions/261022_own-files|own-files]]).
- **The classification is for the reader.** `Surface` and `Silent` change
  nothing the macro emits. A card is still raised where the operation is
  observed ([[map/exarch/io-surface|io-surface]]), and the variant says that it
  is, or why it is not.
- **Platform reach of the lint is the host's.** Windows clippy runs locally
  through `cargo xwin clippy --target x86_64-pc-windows-msvc`; synod is linted
  only there, since the Linux workspace excludes it for its GUI dependencies.
  macOS clippy runs for ral-core and vm-manager (`CC_aarch64_apple_darwin=clang`),
  but ral, exarch and synod need the macOS SDK, so on a host without it their
  macOS-only code is not linted.
- **`project_slug` keys one-to-one.** The conversion at the core of the old slug
  was lossy, so two cwds could share a key. A slug is now a readable part (still
  lossy, for a person scanning a listing), a `-`, and 16 hex digits of the
  SHA-256 of the cwd's exact OS bytes (UTF-16 units on Windows), so two cwds
  that differ at all key apart.

## Rejected

- **A shared crate for the macro.** The expectation is placed by a macro from
  another crate and is never reported as unfulfilled; the stale check is the
  point.
- **A list of variants (`ALL`) or a registry.** A second statement of the set
  can disagree with the first; `dead_code` on the enum states the one fact
  worth stating, that every variant is used.
- **An attribute on a function, or on a statement with several calls.** One
  exemption then covers every banned call in its scope, which is the masking
  this removes.
- **Tags in a reason string, scanned.** A scanner reads text, not types or
  scopes, and a reason has to be well-formed to be found at all.

## Where

`lint/site.rs` (the `Site` trait and `site!`); each crate's `src/site.rs`
(`Surface` and `Silent`, as the crate needs them), with `core/src/path/site.rs`
for the path module's own and `synod/src/shell/site.rs` for synod's bin;
`core/tests/sites.rs` (the scan, the group check, the workspace-lints check and the
two pinned caller sets); `clippy.toml`
(the bans, and the header that states the discipline); `spelled_parent` and
`utf8` in `core/src/path/lex.rs`, `shown` in `core/src/path/show.rs`,
`project_slug` in `core/src/path/slug.rs`.
