#![allow(clippy::disallowed_methods)]

//! The closed syscall-site set (ADR `260619_surface-reads-writes-execs`,
//! Enforcement §2; ADR `261021_one-path`).
//!
//! The clippy `disallowed-methods` denylist (see `clippy.toml`) bans every
//! filesystem, process and network *constructor*, and the raw wrappers the
//! walk is made of, so the only way to read, write, spawn, dial or exec is
//! under an `#[allow(clippy::disallowed_methods, reason = "…")]`.  Clippy
//! resolves the names, aliases and imports included, and CI runs it on Linux,
//! macOS and Windows, so no call escapes it on any platform's `cfg`.  A
//! syscall site's allow opens its reason with a tag — `[surface:<slug>]` (it
//! raises a card), `[silent:<slug>]` (it raises none, and says why) or
//! `[test]` — and this meta-test guarantees the set of tagged sites is
//! *closed*: no new site lands without a reviewed manifest diff.
//!
//! What neither gate checks is that the allow over a syscall is a *tagged*
//! one.  `disallowed_methods` is one lint for every discipline in
//! `clippy.toml`, and clippy cannot read a reason, so a `path-form:` or
//! `native:` allow over an `fs::read` passes both.  That is left to review,
//! as `clippy.toml` says beside the other disciplines it cannot enforce.
//!
//! The test reads the production `src/` of every workspace crate that judges
//! or hands out names (plus the `build.rs` files) — never the `tests/` trees,
//! which carry blanket `#![allow(clippy::disallowed_methods)]` for test
//! scaffolding — with its line comments dropped, and enforces four facts.
//! Outside `core/src/path/`, nothing resolves a name a grant judges; what is
//! left is a row here, each saying why its name is not a grant's to judge:
//! ral's own binary, cgroup and jail, diagnostics, boot and config files;
//! exarch's and synod's own directories and prompt files; terminal and device
//! plumbing; the guest's bootstrap.  `vm-manager`, a host tool whose names no
//! grant judges, is not walked; its one path ban is a reasoned allow.
//!
//!   1. **Every tagged allow is well-formed.**  An allow whose `reason` begins
//!      with a `[…]` tag must name a known `<kind>` (`surface`, `silent`, or
//!      `test`) and carry a non-empty explanation after the tag — so a site is
//!      always a written decision, never an empty marker.  An untagged allow
//!      belongs to another discipline (`path-form:`, `native:`, env, locks,
//!      exit) and is not this test's concern.
//!   2. **The site set equals the reviewed manifest.**  The set of
//!      `(crate-relative file, tag)` pairs for `surface`/`silent` sites must
//!      equal [`SYSCALL_SITES`], and each pair must occur *exactly once*.  A
//!      new site (or a removed/renamed one) fails until a human updates the
//!      manifest — the review gate.  Uniqueness is what keeps that gate shut:
//!      the manifest is keyed by pair, so a site reusing a slug its file
//!      already carries would land with no manifest diff at all.
//!   3. **The string containment helpers gain no caller.**
//!      [`STRING_CONTAINMENT_CALLERS`] pins, outside `path/`, each file that
//!      calls `starts_with_identity`, `path_within_str`, `proper_ancestors` or
//!      `Rendered::{holds, under, rebased}`, and how often.  They stay for
//!      the sandbox renderers that call them today; the plan that moves those
//!      renderers onto `RealPath` deletes the helpers, and finds no new caller.
//!      A ban could not say this: clippy scopes a ban to no module, and counts
//!      nothing.
//!   4. **The ungoverned door gains no caller.**  [`UNGOVERNED_CALLERS`] pins
//!      each production file that reads through `capability::Ungoverned`, the
//!      user's own authority with no grant over it, and how often: a read
//!      ral makes for itself (its own tree, a manifest, a review), never one
//!      a grant judges.
//!
//! ## Updating the manifest when you add a legitimate new site
//!
//! 1. Add the call site and its `#[allow(clippy::disallowed_methods, reason =
//!    "[surface:<slug>] …")]` (or `[silent:<slug>]`), with a `<slug>` unique
//!    within its file — enforced, not merely asked for.  Twins split by
//!    platform take the base slug and a suffix (`…-windows`, `…-linux`), so
//!    the `cfg` a site lives under is legible in the manifest.  The reason is
//!    one line saying what the name is and why it is not a grant's to judge.
//! 2. Add the `(file, "<kind>:<slug>")` pair to [`SYSCALL_SITES`] below, in
//!    the right crate block.
//! 3. This is the review gate: the diff to `SYSCALL_SITES` is where a reviewer
//!    confirms the new site is genuinely one (and surfacing/silent as
//!    claimed), not an accidental ungated open.
//!
//! Test-mod allows (`[test]`, on a `#[cfg(test)] mod tests`) are *not*
//! manifest entries — test fs/process use is blanket-allowed — but they must
//! still carry the tag so fact (1) holds uniformly.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The reviewed set: `(crate-relative path, tag)` for every `surface` and
/// `silent` syscall site in production source, one entry per site.  Tags are
/// stable across line shifts — keyed by file + slug, never line number — so
/// editing a site's file does not flake the test; only adding/removing/renaming
/// a site does.
///
/// Keep this sorted by path for review-friendly diffs.
const SYSCALL_SITES: &[(&str, &str)] = &[
    // ── core ──────────────────────────────────────────────────────────────
    ("core/src/builtins/misc.rs", "silent:ask-tty"),
    ("core/src/builtins/modules.rs", "silent:module-load"),
    ("core/build.rs", "silent:prelude-bake-build"),
    ("core/src/invocation.rs", "silent:argv-before-main"),
    ("core/src/boot.rs", "silent:prelude-bake"),
    // The fs sites: `path/walk/` is the one place that opens a model-named
    // object — the engine in `walk.rs` asks a host module and holds none
    // itself — so every site below is a step of the locate-then-open recipe.
    ("core/src/path/git.rs", "silent:git-dot-git"),
    ("core/src/path/git.rs", "silent:git-root"),
    ("core/src/path/walk/linux.rs", "silent:walk-link-read"),
    ("core/src/path/walk/linux.rs", "silent:walk-root"),
    ("core/src/path/walk/linux.rs", "silent:walk-stat"),
    ("core/src/path/walk/linux.rs", "silent:walk-step"),
    ("core/src/path/walk/linux.rs", "surface:walk-reopen"),
    ("core/src/path/walk/macos.rs", "silent:walk-link-read"),
    ("core/src/path/walk/macos.rs", "silent:walk-root"),
    ("core/src/path/walk/macos.rs", "silent:walk-stat"),
    ("core/src/path/walk/macos.rs", "silent:walk-step"),
    ("core/src/path/walk/macos.rs", "surface:walk-reopen"),
    ("core/src/path/walk/unix.rs", "silent:discard-device"),
    ("core/src/path/walk/unix.rs", "silent:walk-abandon"),
    ("core/src/path/walk/unix.rs", "silent:walk-access"),
    ("core/src/path/walk/unix.rs", "silent:walk-chmod"),
    ("core/src/path/walk/unix.rs", "silent:walk-list"),
    ("core/src/path/walk/unix.rs", "silent:walk-mkdir"),
    ("core/src/path/walk/unix.rs", "silent:walk-reopen-dir"),
    ("core/src/path/walk/unix.rs", "surface:walk-create"),
    ("core/src/path/walk/unix.rs", "surface:walk-publish"),
    ("core/src/path/walk/unix.rs", "surface:walk-stage"),
    ("core/src/path/walk/windows.rs", "silent:discard-device"),
    ("core/src/path/walk/windows.rs", "silent:walk-abandon"),
    ("core/src/path/walk/windows.rs", "silent:walk-access"),
    ("core/src/path/walk/windows.rs", "silent:walk-link-read"),
    ("core/src/path/walk/windows.rs", "silent:walk-list"),
    ("core/src/path/walk/windows.rs", "silent:walk-mkdir"),
    ("core/src/path/walk/windows.rs", "silent:walk-root"),
    ("core/src/path/walk/windows.rs", "silent:walk-step"),
    ("core/src/path/walk/windows.rs", "silent:walk-volume"),
    ("core/src/path/walk/windows.rs", "surface:walk-create"),
    ("core/src/path/walk/windows.rs", "surface:walk-publish"),
    ("core/src/path/walk/windows.rs", "surface:walk-reopen"),
    ("core/src/path/walk/windows.rs", "surface:walk-stage"),
    ("core/src/runtime/command/head.rs", "silent:head-refusal"),
    ("core/src/runtime/command/head.rs", "silent:head-suffixed"),
    ("core/src/path/which.rs", "silent:which-stat"),
    ("core/src/path/which.rs", "silent:which-stat-absent"),
    ("core/src/process/jail/linux.rs", "silent:jail-cgroup-probe"),
    ("core/src/process/jail/linux.rs", "silent:jail-cgroup-procs"),
    ("core/src/process/jail/linux.rs", "silent:jail-cgroup-write"),
    ("core/src/process/jail/linux.rs", "silent:jail-seq-file"),
    ("core/src/process/launch.rs", "silent:nul-device"),
    ("core/src/process/launch.rs", "surface:process-launch"),
    ("core/src/process/spawn_lock.rs", "silent:cloexec-pipe"),
    (
        "core/src/process/spawn_lock.rs",
        "silent:cloexec-socketpair",
    ),
    ("core/src/process/spawn_lock.rs", "silent:spawn-door"),
    ("core/src/sandbox/reexec.rs", "silent:self-reexec-windows"),
    ("core/src/sandbox.rs", "silent:restricted-envelope-probe"),
    ("core/src/sandbox/reexec.rs", "silent:self-reexec"),
    ("core/src/sandbox/carriers.rs", "silent:carrier-shebang"),
    ("core/src/sandbox/carriers.rs", "silent:carrier-system-only"),
    ("core/src/process/tree.rs", "silent:ps-sample"),
    ("core/src/sandbox/diag/linux.rs", "silent:journal-read"),
    ("core/src/sandbox/diag/macos.rs", "silent:log-show"),
    ("core/src/sandbox/linux.rs", "silent:own-cgroup"),
    ("core/src/sandbox/linux/host.rs", "silent:bwrap-host-probe"),
    ("core/src/sandbox/reexec.rs", "silent:pin-locate"),
    ("core/src/sandbox/reexec.rs", "silent:pin-open"),
    ("core/src/sandbox/reexec.rs", "silent:pinned-exec"),
    ("core/src/sandbox/warrant.rs", "silent:respawn-exec"),
    ("core/src/sandbox/warrant.rs", "silent:respawn-spawn"),
    (
        "core/src/sandbox/windows/dacl/frozen.rs",
        "silent:dacl-frozen-link",
    ),
    (
        "core/src/sandbox/windows/dacl/ledger.rs",
        "silent:dacl-ledger-quarantine",
    ),
    (
        "core/src/sandbox/windows/dacl/ledger.rs",
        "silent:dacl-ledger-read",
    ),
    (
        "core/src/sandbox/windows/dacl/ledger.rs",
        "silent:dacl-ledger-remove",
    ),
    (
        "core/src/sandbox/windows/dacl/ledger.rs",
        "silent:dacl-ledger-sweep",
    ),
    (
        "core/src/sandbox/windows/dacl/ledger.rs",
        "silent:dacl-ledger-write",
    ),
    (
        "core/src/sandbox/windows/dacl/stamp.rs",
        "silent:dacl-stamp-read",
    ),
    (
        "core/src/sandbox/windows/dacl/ledger.rs",
        "silent:dacl-state-dir",
    ),
    (
        "core/src/sandbox/windows/dacl/ledger.rs",
        "silent:dacl-state-dir-recheck",
    ),
    ("core/src/frame.rs", "silent:frame-dump"),
    ("core/src/frame.rs", "silent:frame-dump-nonunix"),
    ("core/src/uutils.rs", "silent:diff-read"),
    ("core/src/protocol/channel.rs", "silent:wire-pair-windows"),
    // ── guest-net ─────────────────────────────────────────────────────────
    ("guest-net/src/vet.rs", "silent:vet-dial"),
    ("guest-net/src/vet.rs", "silent:vet-resolve"),
    // ── exarch ────────────────────────────────────────────────────────────
    ("exarch/src/app.rs", "silent:log-run-dir"),
    ("exarch/src/app.rs", "silent:resume-list"),
    ("exarch/src/app.rs", "silent:run-lock"),
    ("exarch/src/app.rs", "silent:scratch-bootstrap"),
    ("exarch/src/app.rs", "silent:scratch-drop"),
    ("exarch/src/app.rs", "silent:scratch-reap"),
    ("exarch/src/cli.rs", "silent:seed-file"),
    ("exarch/src/config.rs", "silent:config-load"),
    ("exarch/src/lib.rs", "silent:export-target"),
    ("exarch/src/egress.rs", "silent:net-audit-open"),
    ("exarch/src/egress.rs", "silent:net-audit-rotate"),
    ("exarch/src/egress.rs", "silent:net-audit-size"),
    ("exarch/src/record/session.rs", "silent:record-size"),
    ("exarch/src/record/session.rs", "silent:resume-record"),
    ("exarch/src/record/session.rs", "silent:session-dir"),
    ("exarch/src/record/session.rs", "silent:session-ids"),
    ("exarch/src/prompt/host.rs", "silent:git-launch"),
    ("exarch/src/record/log.rs", "silent:record-file-append"),
    ("exarch/src/record/log.rs", "silent:record-file-create"),
    ("exarch/src/record/log.rs", "silent:record-file-read"),
    ("exarch/src/record/log.rs", "silent:record-replay-read"),
    ("exarch/src/record/log.rs", "silent:record-complete-len"),
    (
        "exarch/src/record/model/resume.rs",
        "silent:model-fold-crash-quarantine",
    ),
    (
        "exarch/src/record/model/transcript.rs",
        "silent:model-fold-pointer-read",
    ),
    ("exarch/src/provider/models.rs", "silent:models-cache-read"),
    ("exarch/src/provider/models.rs", "silent:models-cache-write"),
    ("exarch/src/provider/pricing.rs", "silent:pricing-client"),
    ("exarch/src/provider/tls.rs", "silent:provider-client"),
    (
        "exarch/src/provider/oauth/browser.rs",
        "silent:browser-launch",
    ),
    (
        "exarch/src/provider/oauth/browser.rs",
        "silent:browser-launch-linux",
    ),
    (
        "exarch/src/provider/oauth/browser.rs",
        "silent:callback-listener",
    ),
    ("exarch/src/provider/oauth/mod.rs", "silent:oauth-client"),
    ("exarch/src/provider/oauth/mod.rs", "silent:token-dir"),
    ("exarch/src/provider/oauth/mod.rs", "silent:token-read"),
    ("exarch/src/provider/oauth/mod.rs", "silent:token-remove"),
    // The owner-only secret writer the ChatGPT token store and the credential
    // fallback file share.
    (
        "exarch/src/provider/secret_file.rs",
        "silent:secret-create-windows",
    ),
    ("exarch/src/provider/secret_file.rs", "silent:secret-write"),
    (
        "exarch/src/provider/secret_file.rs",
        "silent:secret-write-windows",
    ),
    // The fallback key file, written only where this computer offers no
    // credential manager to keep the key in instead.
    ("exarch/src/provider/keychain.rs", "silent:key-read"),
    ("exarch/src/provider/keychain.rs", "silent:key-write"),
    // Provider declarations written back to the app's own config directory by
    // synod's accounts screen; addresses and protocols, never keys.
    ("exarch/src/config.rs", "silent:config-write"),
    ("exarch/src/provider/state.rs", "silent:state-read"),
    ("exarch/src/provider/state.rs", "silent:state-write"),
    ("exarch/src/tui/terminal.rs", "silent:editor-compose"),
    ("exarch/src/tui/terminal.rs", "silent:stderr-log"),
    ("exarch/src/tui/terminal.rs", "silent:stderr-log-windows"),
    ("exarch/src/tui/commands.rs", "silent:export-overwrite"),
    ("exarch/src/tui/export.rs", "silent:export"),
    // ── ral ───────────────────────────────────────────────────────────────
    ("ral/build.rs", "silent:git-probe"),
    ("ral/src/batch.rs", "silent:script-read"),
    ("ral/src/bridge.rs", "silent:respawn-posix-sh"),
    ("ral/src/platform.rs", "silent:exit-hints-read"),
    ("ral/src/repl/config.rs", "silent:history-mkdir"),
    ("ral/src/repl/config.rs", "silent:rc-candidate"),
    ("ral/src/repl/config.rs", "silent:rc-write"),
    ("ral/src/repl/config/source.rs", "silent:config-read"),
    ("ral/src/repl/config/source.rs", "silent:rc-exists"),
    ("ral/src/repl/session/boot.rs", "silent:crashlog-write"),
    // ── synod ─────────────────────────────────────────────────────────────
    // Synod's own boot media, rootfs cache, prompt files and diagnostics, and
    // the folder the user picked, which the root stack judges: none of these
    // names is a grant's to judge.
    ("synod/src/boot.rs", "silent:boot-media"),
    ("synod/src/boot.rs", "silent:rootfs-cache-dir"),
    ("synod/src/boot.rs", "silent:rootfs-checksum-read"),
    ("synod/src/boot.rs", "silent:rootfs-marker-read"),
    ("synod/src/boot.rs", "silent:rootfs-marker-write"),
    ("synod/src/machine_factory.rs", "silent:wire-pair-windows"),
    ("synod/src/prompt.rs", "silent:house-rules"),
    ("synod/src/session/engine_log.rs", "silent:engine-log"),
    ("synod/src/shell/commands.rs", "silent:mark-of-the-web"),
];

/// Production source roots, crate-relative to the workspace root.  The
/// vendored `ral-ripgrep-core` opts out of the workspace clippy lints (it
/// stays upstream-diffable) so it is *not* governed by this discipline and is
/// excluded here.  `guest-net` is here because the CONNECT proxy is on the
/// model's own egress path, and `synod` because its review hands a folder and
/// a name to the OS; `vm-manager` is a host tool whose names no grant judges.
const SRC_ROOTS: &[&str] = &[
    "core/src",
    "exarch/src",
    "guest-net/src",
    "ral/src",
    "synod/src",
];

/// Build scripts are production code too, and `ral/build.rs` spawns `git`.
const BUILD_SCRIPTS: &[&str] = &["core/build.rs", "ral/build.rs", "exarch/build.rs"];

/// The string containment helpers, which `path/` exports for the sandbox
/// renderers that still hold names as strings: each file outside `path/` that
/// calls one, with the helper and how many times.  `Rendered::{holds, under,
/// rebased}` are counted as `.holds(`, `.under(` and `.rebased(` in a file
/// that names `Rendered`.  Nothing new may join this list.
const STRING_CONTAINMENT_CALLERS: &[(&str, &str, usize)] = &[
    ("core/src/sandbox/linux.rs", ".holds(", 2),
    ("core/src/sandbox/linux.rs", ".rebased(", 1),
    ("core/src/sandbox/linux/landlock.rs", ".holds(", 1),
    ("core/src/sandbox/linux/landlock.rs", ".under(", 1),
    ("core/src/sandbox/macos.rs", "proper_ancestors", 2),
];

/// What `Rendered` lends a renderer, counted in a file that names it.
const RENDERED_NAME_HELPERS: &[&str] = &[".holds(", ".under(", ".rebased("];

/// Every production file that reads through `Ungoverned`, and how often.
const UNGOVERNED_CALLERS: &[(&str, usize)] = &[
    ("core/src/guard/freeze.rs", 1),
    ("core/src/path/tree.rs", 1),
    ("core/src/sandbox/carriers.rs", 1),
    ("synod/src/grant.rs", 1),
    ("synod/src/shell/review.rs", 3),
    ("synod/src/workspace/manifest.rs", 2),
];

/// The helpers `STRING_CONTAINMENT_CALLERS` counts, by name.
const STRING_CONTAINMENT_HELPERS: &[&str] = &[
    "starts_with_identity",
    "path_within_str",
    "proper_ancestors",
];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `…/core` at compile time; the workspace root is
    // its parent.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core/ has a parent (the workspace root)")
        .to_path_buf()
}

/// Recursively collect every production `.rs` file under `dir`: a unit-test
/// module lives out of line as `tests.rs` or under a `tests/` directory, and
/// is scaffolding, not a site.
fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "tests" {
                rs_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") && name != "tests.rs" {
            out.push(path);
        }
    }
}

/// Every production source file (src roots + build scripts), as its
/// crate-relative path and its code: the text with its line comments, doc
/// comments among them, dropped, so a name a comment mentions is neither a
/// site nor a caller.
fn production_sources() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut abs = Vec::new();
    for src in SRC_ROOTS {
        rs_files(&root.join(src), &mut abs);
    }
    abs.extend(
        BUILD_SCRIPTS
            .iter()
            .map(|bs| root.join(bs))
            .filter(|p| p.exists()),
    );
    abs.into_iter()
        .map(|path| {
            let rel = path
                .strip_prefix(&root)
                .expect("file is under the workspace root")
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
            let code = text
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            (rel, code)
        })
        .collect()
}

/// The `reason` of every `clippy::disallowed_methods` allow in `code`, `None`
/// for an allow that gives none.  What follows the lint's name runs to the
/// attribute's `)` unless a `reason = "…"` comes first.
fn allow_reasons(code: &str) -> Vec<Option<String>> {
    code.split("clippy::disallowed_methods")
        .skip(1)
        .map(|rest| {
            let end = rest.find(')').unwrap_or(rest.len());
            let rest = &rest[rest[..end].find("reason")?..];
            let open = rest.find('"')? + 1;
            let mut close = open;
            while let Some(&c) = rest.as_bytes().get(close)
                && c != b'"'
            {
                close += if c == b'\\' { 2 } else { 1 };
            }
            Some(rest[open..close.min(rest.len())].to_string())
        })
        .collect()
}

/// A parsed `[<kind>[:<slug>]]` tag plus the explanation after it.
struct Tag {
    kind: String,
    /// Full `<kind>:<slug>` token (without brackets); `test` for the test kind.
    full: String,
    explanation: String,
}

/// Parse the leading `[…]` tag out of an allow `reason` string.
fn parse_tag(reason: &str) -> Option<Tag> {
    let reason = reason.trim_start();
    let rest = reason.strip_prefix('[')?;
    let close = rest.find(']')?;
    let full = rest[..close].to_string(); // e.g. "silent:which-stat" or "test"
    let explanation = rest[close + 1..].trim().to_string();
    let kind = full.split(':').next().unwrap_or("").to_string();
    Some(Tag {
        kind,
        full,
        explanation,
    })
}

#[test]
fn the_tagged_sites_are_the_reviewed_manifest() {
    let mut failures = Vec::new();
    // Counted, not just collected: a set would swallow a slug reused inside one
    // file, and that reuse is exactly the way a new site lands with no manifest
    // diff to review.
    let mut found: BTreeMap<(String, String), usize> = BTreeMap::new();

    for (rel, code) in production_sources() {
        // Fact (1): every tagged allow is well-formed (known kind, non-empty
        // explanation).
        for tag in allow_reasons(&code)
            .iter()
            .flatten()
            .filter_map(|r| parse_tag(r))
        {
            if !matches!(tag.kind.as_str(), "surface" | "silent" | "test") {
                failures.push(format!(
                    "{rel}: unknown kind `{}` (expected surface|silent|test) in `[{}]`.",
                    tag.kind, tag.full
                ));
            }
            if tag.explanation.is_empty() {
                failures.push(format!(
                    "{rel}: tag `[{}]` has an empty explanation after it.",
                    tag.full
                ));
            }
            if tag.kind == "surface" || tag.kind == "silent" {
                *found.entry((rel.clone(), tag.full)).or_default() += 1;
            }
        }
    }

    // Fact (2): the surface/silent site set equals the reviewed manifest, one
    // entry per site.  Uniqueness first: without it a reused slug is a site the
    // manifest already names, and the diff a reviewer reads stays empty.
    for ((f, t), n) in &found {
        if *n > 1 {
            failures.push(format!(
                "{f}: {n} sites share the tag [{t}]\n    → a manifest entry names one site, so give each its own slug (platform twins take a -windows / -linux suffix)."
            ));
        }
    }

    let found: BTreeSet<(String, String)> = found.into_keys().collect();
    let manifest: BTreeSet<(String, String)> = SYSCALL_SITES
        .iter()
        .map(|(f, t)| (f.to_string(), t.to_string()))
        .collect();

    for (f, t) in found.difference(&manifest) {
        failures.push(format!(
            "NEW unaccounted syscall site {f}  [{t}]\n    → review it, then add the pair to SYSCALL_SITES in this file."
        ));
    }
    for (f, t) in manifest.difference(&found) {
        failures.push(format!(
            "STALE manifest entry {f}  [{t}] no longer exists in source\n    → remove it from SYSCALL_SITES."
        ));
    }

    assert!(
        failures.is_empty(),
        "syscall-site invariant violated ({} issue(s)):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn the_ungoverned_door_gains_no_caller() {
    let counted: BTreeMap<String, usize> = production_sources()
        .into_iter()
        .filter(|(rel, _)| rel != "core/src/capability/fs.rs")
        .map(|(rel, code)| {
            let n = code.matches("Ungoverned::").count();
            (rel, n)
        })
        .filter(|(_, n)| *n > 0)
        .collect();
    let pinned: BTreeMap<String, usize> = UNGOVERNED_CALLERS
        .iter()
        .map(|(f, n)| (f.to_string(), *n))
        .collect();
    assert_eq!(
        counted, pinned,
        "a read through `Ungoverned` was added or removed: a read a grant should judge goes \
         through `GrantStack::judge`; update UNGOVERNED_CALLERS only for ral's own reads."
    );
}

#[test]
fn the_string_containment_helpers_gain_no_caller() {
    let mut counted: BTreeMap<(String, &str), usize> = BTreeMap::new();

    for (rel, code) in production_sources() {
        if rel.starts_with("core/src/path/") || rel == "core/src/path.rs" {
            continue;
        }
        let rendered = code.contains("Rendered");
        for name in STRING_CONTAINMENT_HELPERS
            .iter()
            .chain(RENDERED_NAME_HELPERS.iter().filter(|_| rendered))
        {
            let n = code.matches(name).count();
            if n > 0 {
                counted.insert((rel.clone(), name), n);
            }
        }
    }

    let pinned: BTreeMap<(String, &str), usize> = STRING_CONTAINMENT_CALLERS
        .iter()
        .map(|(f, name, n)| ((f.to_string(), *name), *n))
        .collect();
    assert_eq!(
        counted, pinned,
        "a string containment helper gained or lost a caller outside path/: the renderers take \
         RealPath::{{contains, rebased, ancestors_within}} instead, and the sequel plan deletes the \
         helpers — update STRING_CONTAINMENT_CALLERS only when a caller goes."
    );
}
