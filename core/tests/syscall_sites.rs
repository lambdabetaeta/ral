#![allow(clippy::disallowed_methods)]

//! The closed syscall-site set (ADR `260619_surface-reads-writes-execs`,
//! Enforcement §2; ADR `261021_one-path`).
//!
//! The clippy `disallowed-methods` denylist (see `clippy.toml`) bans every
//! filesystem and process *constructor* outside a handful of reviewed syscall
//! sites, so the only way to read, write, spawn, or exec is at a site that
//! either *surfaces* the operation as a card or is a *reasoned-silent* one.
//! Each site carries an `#[allow(clippy::disallowed_methods, reason = "[…] …")]`
//! whose tag records which kind it is.  Clippy guarantees there is no I/O
//! *outside* a site; this meta-test guarantees the set of sites is *closed*:
//! no new, unaccounted site can slip in unreviewed.
//!
//! It walks the production `src/` of every workspace crate that judges or
//! hands out names (plus the `build.rs` files) — never the `tests/` trees,
//! which carry blanket `#![allow(clippy::disallowed_methods)]` for test
//! scaffolding, a separate category that is not a syscall site — and enforces
//! five facts.  Outside `core/src/path/`, nothing resolves a name a grant
//! judges; what is left is a row here, each saying why its name is not a
//! grant's to judge: ral's own binary, cgroup and jail, diagnostics, boot and
//! config files; exarch's and synod's own directories and prompt files;
//! terminal and device plumbing; the guest's bootstrap.  `vm-manager`, a host
//! tool whose names no grant judges, is not walked; its one path ban is a
//! reasoned allow.
//!
//!   1. **Every tagged allow is well-formed.**  An allow whose `reason` begins
//!      with a `[…]` tag must name a known `<kind>` (`surface`, `silent`, or
//!      `test`) and carry a non-empty explanation after the tag — so a site is
//!      always a written decision, never an empty marker.  (Pre-existing bare
//!      `#[allow(clippy::disallowed_methods)]` for the *other* disciplines —
//!      path-construction, cwd, child-wait, env, `path-form:` lexical work on a
//!      spelling — are governed by their own denylist entries and are out of
//!      scope here; this test concerns the fs/process syscall sites.)
//!   2. **The site set equals the reviewed manifest.**  The set of
//!      `(crate-relative file, tag)` pairs for `surface`/`silent` sites must
//!      equal [`SYSCALL_SITES`], and each pair must occur *exactly once*.  A
//!      new site (or a removed/renamed one) fails until a human updates the
//!      manifest — the review gate.  Uniqueness is what keeps that gate shut:
//!      the manifest is keyed by pair, so a site reusing a slug its file
//!      already carries would land with no manifest diff at all.
//!   3. **Every call lies in a tagged site.**  A *call* is an occurrence of a
//!      banned token in code: not in a comment, a string or a character
//!      literal, and not under `#[cfg(test)]`, which is scaffolding as an
//!      out-of-line `tests.rs` is.  A tagged allow covers its *item*: from the
//!      attribute to the item's closing brace (a statement, to its `;`).  A
//!      call that no tagged item contains fails, so a file with one tagged
//!      allow cannot hide a second constructor.  It also catches
//!      platform-`cfg`-gated sites a single-target clippy run on one OS would
//!      miss.
//!   4. **A single-door token appears only at its door.**  Fact (3) is met by
//!      any tagged allow, so for a token with exactly one legitimate caller,
//!      [`SINGLE_DOORS`] names that file and no other production file may
//!      contain the token at all.
//!   5. **The string containment helpers gain no caller.**
//!      [`STRING_CONTAINMENT_CALLERS`] pins, outside `path/`, each file that
//!      calls `starts_with_identity`, `path_within_str`, `Rendered::holds` or
//!      `proper_ancestors`, and how often.  They stay for the sandbox
//!      renderers that call them today; the plan that moves those renderers
//!      onto `RealPath` deletes the helpers, and finds no new caller.
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
    ("core/src/path/walk/macos.rs", "silent:walk-stored-name"),
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
    ("core/src/path/git.rs", "silent:git-read"),
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

/// Banned fs/process/network constructor tokens (substring match).  Coarse on
/// purpose: fact (3) only needs "this code makes such a call", not clippy's
/// resolution.
const BANNED_TOKENS: &[&str] = &[
    "fs::File::open",
    "fs::File::create",
    "fs::OpenOptions::",
    "fs::read(",
    "fs::read_to_string",
    "fs::write(",
    "fs::read_dir",
    "fs::metadata",
    "fs::symlink_metadata",
    "fs::read_link",
    "fs::remove_file",
    "fs::remove_dir_all",
    "fs::create_dir_all",
    "fs::rename(",
    "fs::copy(",
    "fs::set_permissions",
    // The raw wrappers the walk is made of.  A walk imports `openat2` and then
    // calls it bare, so `bare_calls` finds those too.
    "fs::open(",
    "fs::openat(",
    "fs::openat2(",
    "fs::statat(",
    "fs::accessat(",
    "fs::readlinkat(",
    "fs::renameat(",
    "fs::unlinkat(",
    "fs::mkdirat(",
    "libc::open(",
    "libc::openat(",
    "CreateFileW",
    "NtCreateFile",
    "GetFinalPathNameByHandleW",
    "GetNamedSecurityInfoW",
    "SetNamedSecurityInfoW",
    // The process type is named by its path or import, not bare `Command::new`,
    // which also spells the audit fact `fact::Command`; an import is caught by
    // `imports_banned`, and its bare calls by `bare_calls`.
    "process::Command::new",
    "CommandExt::exec",
    // The pipe door: `cloexec_pipe` closes a CLOEXEC window Apple leaves open
    // between `pipe()` and the flag, so a raw `os_pipe::pipe()` is a site too.
    // Listed here rather than left to clippy because the one that got through
    // was `cfg(windows)`, which a Unix clippy run never compiles.
    "os_pipe::pipe(",
    // The cap-std twins are imported by name and then called bare, so the
    // import betrays them (`imports_banned`).
    "cap_primitives::fs::",
    "cap_fs_ext::",
    // The network family: a host:port is an outside name and the socket it
    // returns is the capability, the same shape as an open.  `reqwest` is
    // spelled in full because `Client::builder` bare is genai's client, which
    // threads this crate's own preconfigured reqwest client rather than
    // acquiring anything.
    "TcpStream::connect",
    "TcpListener::bind",
    "UdpSocket::bind",
    "to_socket_addrs",
    "reqwest::Client::builder",
    "reqwest::Client::new",
];

/// The raw `rustix::fs` wrappers, the names a walk imports and then calls bare.
const RAW_WRAPPERS: &[&str] = &[
    "open",
    "openat",
    "openat2",
    "statat",
    "accessat",
    "readlinkat",
    "renameat",
    "unlinkat",
    "mkdirat",
];

/// The names `use` items import from a module that makes calls by name: the
/// wrappers of `rustix::fs`, `Command` of `std::process`, the functions of
/// `cap_primitives::fs`.  Each is a call wherever the file then uses it bare.
fn bare_calls(code: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (at, import) in use_items(code) {
        let words: Vec<&str> = import
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .collect();
        let names: Vec<&str> = if import.contains("rustix::fs") {
            words
                .iter()
                .copied()
                .filter(|w| RAW_WRAPPERS.contains(w))
                .collect()
        } else if import.contains("cap_primitives::fs") || import.contains("cap_fs_ext") {
            words
                .iter()
                .copied()
                .filter(|w| w.starts_with(char::is_lowercase))
                .collect()
        } else {
            Vec::new()
        };
        for name in names.iter().filter(|n| {
            ![
                "use",
                "pub",
                "crate",
                "self",
                "super",
                "std",
                "rustix",
                "fs",
                "cap_primitives",
                "cap_fs_ext",
                "as",
            ]
            .contains(n)
        }) {
            found.extend(bare_uses(code, &format!("{name}(")).map(|at| (at, format!("{name}(…)"))));
        }
        if import.contains("process") && words.contains(&"Command") {
            found.extend(
                bare_uses(code, "Command::new(").map(|at| (at, "Command::new".to_string())),
            );
        }
        let _ = at;
    }
    found
}

/// Where `token` occurs in `code` beginning a name: after neither an
/// identifier character, a `.` nor a `:`.
fn bare_uses<'a>(code: &'a str, token: &'a str) -> impl Iterator<Item = usize> + 'a {
    code.match_indices(token)
        .map(|(at, _)| at)
        .filter(move |&at| {
            at == 0
                || !(code.as_bytes()[at - 1].is_ascii_alphanumeric()
                    || b"_.:".contains(&code.as_bytes()[at - 1]))
        })
}

/// Every `use` item of `code`, as its offset and text.
fn use_items(code: &str) -> Vec<(usize, &str)> {
    code.match_indices("use ")
        .filter(|&(at, _)| {
            let before = code[..at].trim_end();
            before.is_empty()
                || before.ends_with([';', '{', '}', ']'])
                || before.ends_with("pub")
                || before.ends_with(')')
        })
        .filter_map(|(at, _)| code[at..].find(';').map(|n| (at, &code[at..at + n])))
        .collect()
}

/// Whether some `use` item of `code` names a banned token: an import is not a
/// call, but a file that imports one makes calls by name.
fn imports_banned(code: &str) -> Option<&'static str> {
    use_items(code).into_iter().find_map(|(_, import)| {
        let tok = BANNED_TOKENS.iter().find(|t| import.contains(**t)).copied();
        tok.or_else(|| {
            (import.contains("process")
                && import
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|w| w == "Command"))
            .then_some("process::Command")
        })
        .or_else(|| {
            (import.contains("rustix::fs")
                && import
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|w| RAW_WRAPPERS.contains(&w)))
            .then_some("rustix::fs::{…}")
        })
    })
}

/// A banned token restricted to exactly one production file: the token may
/// appear in no production file but its door, tagged allow or not.
const SINGLE_DOORS: &[(&str, &str)] = &[("os_pipe::pipe(", "core/src/process/spawn_lock.rs")];

/// The string containment helpers, which `path/` exports for the sandbox
/// renderers that still hold names as strings: each file outside `path/` that
/// calls one, with the helper and how many times.  `Rendered::holds` is
/// counted as `.holds(` in a file that names `Rendered`.  Nothing new may
/// join this list.
const STRING_CONTAINMENT_CALLERS: &[(&str, &str, usize)] = &[
    ("core/src/sandbox/linux.rs", ".holds(", 2),
    ("core/src/sandbox/linux/landlock.rs", ".holds(", 2),
    ("core/src/sandbox/macos.rs", "proper_ancestors", 2),
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

/// Every production `.rs` file (src roots + build scripts), crate-relative.
fn production_files(root: &Path) -> Vec<PathBuf> {
    let mut abs = Vec::new();
    for src in SRC_ROOTS {
        rs_files(&root.join(src), &mut abs);
    }
    for bs in BUILD_SCRIPTS {
        let p = root.join(bs);
        if p.exists() {
            abs.push(p);
        }
    }
    abs.into_iter()
        .map(|p| {
            p.strip_prefix(root)
                .expect("file is under the workspace root")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .map(PathBuf::from)
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

/// `text` with every comment, string and character literal blanked to spaces
/// (newlines kept), so offsets still match `text` and a token inside one is no
/// call.  The quotes stay, so an attribute's `reason = ""` still reads as one.
fn blanked(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let wipe = |out: &mut [u8], from: usize, to: usize| {
        out[from..to]
            .iter_mut()
            .filter(|c| **c != b'\n')
            .for_each(|c| *c = b' ');
    };
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = text[i..].find('\n').map_or(b.len(), |n| i + n);
                wipe(&mut out, i, end);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let (mut depth, mut j) = (1, i + 2);
                while j < b.len() && depth > 0 {
                    match (b[j], b.get(j + 1)) {
                        (b'/', Some(b'*')) => (depth, j) = (depth + 1, j + 2),
                        (b'*', Some(b'/')) => (depth, j) = (depth - 1, j + 2),
                        _ => j += 1,
                    }
                }
                wipe(&mut out, i, j);
                i = j;
            }
            b'r' if (i == 0
                || !ident(b[i - 1])
                || (b[i - 1] == b'b' && (i < 2 || !ident(b[i - 2]))))
                && b[i + 1..].iter().find(|c| **c != b'#') == Some(&b'"') =>
            {
                let hashes = b[i + 1..].iter().take_while(|c| **c == b'#').count();
                let open = i + 1 + hashes;
                let close = format!("\"{}", "#".repeat(hashes));
                let end = text[open + 1..]
                    .find(&close)
                    .map_or(b.len(), |n| open + 1 + n);
                wipe(&mut out, open + 1, end);
                i = (end + close.len()).min(b.len());
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let j = j.min(b.len());
                wipe(&mut out, i + 1, j);
                i = j + 1;
            }
            b'\'' => {
                let close = if b.get(i + 1) == Some(&b'\\') {
                    text[i + 3..].find('\'').map(|n| i + 3 + n)
                } else {
                    text[i + 1..]
                        .char_indices()
                        .nth(1)
                        .filter(|&(n, c)| c == '\'' && n > 0)
                        .map(|(n, _)| i + 1 + n)
                };
                match close {
                    Some(end) => {
                        wipe(&mut out, i + 1, end);
                        i = end + 1;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).expect("only ASCII delimiters bound what is blanked")
}

/// The index of the bracket closing the one at `open`, or the end of `code`.
fn closing(code: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    for (j, c) in code.iter().enumerate().skip(open) {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
    }
    code.len()
}

/// Where the attributes beginning at or after `from` end.
fn past_attributes(code: &[u8], mut from: usize) -> usize {
    loop {
        while code.get(from).is_some_and(u8::is_ascii_whitespace) {
            from += 1;
        }
        let bang = usize::from(code.get(from + 1) == Some(&b'!'));
        if code[from..].starts_with(b"#") && code.get(from + 1 + bang) == Some(&b'[') {
            from = closing(code, from + 1 + bang) + 1;
        } else {
            return from;
        }
    }
}

/// The end of the item that begins at `from` (past its attributes): its
/// closing brace, or its `;`.  A statement (`let …`, an expression) ends at
/// its `;`, or where the block around it closes.
fn item_end(code: &[u8], from: usize) -> usize {
    const ITEMS: &[&str] = &[
        "pub",
        "fn",
        "impl",
        "mod",
        "trait",
        "struct",
        "enum",
        "union",
        "static",
        "const",
        "use",
        "type",
        "unsafe",
        "extern",
        "async",
        "macro_rules",
    ];
    let start = past_attributes(code, from).min(code.len());
    let word: String = code[start..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
        .map(|c| *c as char)
        .collect();
    let item = ITEMS.contains(&word.as_str());
    let mut depth = 0usize;
    for (j, c) in code.iter().enumerate().skip(start) {
        match c {
            b'{' if item && depth == 0 => return closing(code, j) + 1,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' if depth == 0 => return j,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth == 0 => return j + 1,
            _ => {}
        }
    }
    code.len()
}

/// A `clippy::disallowed_methods` allow: the span of the item it covers, and
/// its `reason`.
struct Allow {
    covers: std::ops::Range<usize>,
    reason: Option<String>,
}

/// The `reason = "…"` payload of an attribute's text, if present.
fn reason_of(attr: &str) -> Option<String> {
    let after = attr.find("reason")?;
    let eq = attr[after..].find('=')? + after;
    let q1 = attr[eq..].find('"')? + eq + 1;
    let mut q2 = q1;
    while attr.as_bytes().get(q2).is_some_and(|c| *c != b'"') {
        q2 += if attr.as_bytes()[q2] == b'\\' { 2 } else { 1 };
    }
    Some(attr[q1..q2.min(attr.len())].to_string())
}

/// Every attribute naming `clippy::disallowed_methods` in `text`, with the
/// item each covers.  An inner attribute covers the block around it.
fn allows(text: &str, code: &str) -> Vec<Allow> {
    let needle = "clippy::disallowed_methods";
    let c = code.as_bytes();
    code.match_indices(needle)
        .map(|(at, _)| {
            let start = code[..at]
                .rfind("#[")
                .max(code[..at].rfind("#!["))
                .expect("an attribute");
            let bracket = start + code[start..].find('[').expect("`[`");
            let end = closing(c, bracket) + 1;
            let covers = if c[start + 1] == b'!' {
                start..item_end(c, end).max(enclosing_end(c, end))
            } else {
                start..item_end(c, end)
            };
            Allow {
                covers,
                reason: reason_of(&text[start..end.min(text.len())]),
            }
        })
        .collect()
}

/// The end of the block around `from`: its closing brace, or the end of `code`.
fn enclosing_end(code: &[u8], from: usize) -> usize {
    let mut depth = 0usize;
    for (j, c) in code.iter().enumerate().skip(from) {
        match c {
            b'{' => depth += 1,
            b'}' if depth == 0 => return j,
            b'}' => depth -= 1,
            _ => {}
        }
    }
    code.len()
}

/// `code` with each item gated on `#[cfg(test)]` blanked: scaffolding, which a
/// call is not counted in.
fn without_test_items(code: &str) -> String {
    let mut out = code.as_bytes().to_vec();
    for gate in ["#[cfg(test)]", "#[cfg(all(test"] {
        for (at, _) in code.match_indices(gate) {
            let end = item_end(code.as_bytes(), closing(code.as_bytes(), at + 1) + 1);
            out[at..end]
                .iter_mut()
                .filter(|c| **c != b'\n')
                .for_each(|c| *c = b' ');
        }
    }
    String::from_utf8(out).expect("blanking ASCII spans keeps UTF-8")
}

/// The 1-based line of byte offset `at` in `text`.
fn line_of(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

/// `code` with its `use` items blanked: an import is not a call.
fn without_uses(code: &str) -> String {
    let mut out = code.as_bytes().to_vec();
    for (at, import) in use_items(code) {
        out[at..at + import.len()]
            .iter_mut()
            .filter(|c| **c != b'\n')
            .for_each(|c| *c = b' ');
    }
    String::from_utf8(out).expect("blanking ASCII spans keeps UTF-8")
}

/// Every call in `code`, as `(offset, token)`: a banned token anywhere but in
/// a `use` item, and each name a `use` item imports from a module that makes
/// calls by name, where the file then uses it bare.
fn calls(code: &str) -> Vec<(usize, String)> {
    let bare = without_uses(code);
    let mut found: Vec<_> = BANNED_TOKENS
        .iter()
        .flat_map(|tok| {
            bare.match_indices(tok)
                .map(move |(at, _)| (at, (*tok).to_string()))
        })
        .collect();
    found.extend(
        bare_calls(code)
            .into_iter()
            .filter(|(at, _)| bare.is_char_boundary(*at)),
    );
    found.sort_unstable();
    found.dedup();
    found
}

#[test]
fn every_production_disallowed_allow_is_a_tagged_site() {
    let root = workspace_root();
    let mut failures = Vec::new();
    // Counted, not just collected: a set would swallow a slug reused inside one
    // file, and that reuse is exactly the way a new site lands with no manifest
    // diff to review.
    let mut found: BTreeMap<(String, String), usize> = BTreeMap::new();

    for rel in production_files(&root) {
        let rel_str = rel.to_string_lossy().to_string();
        let text = std::fs::read_to_string(root.join(&rel))
            .unwrap_or_else(|e| panic!("read {rel_str}: {e}"));
        let code = blanked(&text);
        let allows = allows(&text, &code);

        // Fact (1): every tagged allow is well-formed (known kind, non-empty
        // explanation).  Untagged allows belong to the other
        // disallowed-methods disciplines and are not this test's concern;
        // fact (3) below is what stops an fs/process site hiding behind one.
        let mut tagged = Vec::new();
        for allow in &allows {
            let Some(tag) = allow.reason.as_deref().and_then(parse_tag) else {
                continue;
            };
            if !matches!(tag.kind.as_str(), "surface" | "silent" | "test") {
                failures.push(format!(
                    "{rel_str}: unknown kind `{}` (expected surface|silent|test) in `[{}]`.",
                    tag.kind, tag.full
                ));
            }
            if tag.explanation.is_empty() {
                failures.push(format!(
                    "{rel_str}: tag `[{}]` has an empty explanation after it.",
                    tag.full
                ));
            }
            if tag.kind == "surface" || tag.kind == "silent" {
                *found.entry((rel_str.clone(), tag.full)).or_default() += 1;
            }
            tagged.push(allow);
        }

        // Fact (3): every call lies inside a tagged allow's item; an import
        // of a banned name, which is no call, needs any tagged allow at all.
        let scan = without_test_items(&code);
        if tagged.is_empty()
            && let Some(tok) = imports_banned(&scan)
        {
            failures.push(format!(
                "{rel_str}: imports a banned fs/process name (`{tok}`) but declares no tagged \
                 allow.\n    → route it through a reviewed syscall site or add a tagged allow."
            ));
        }
        for (at, tok) in calls(&scan) {
            if !tagged.iter().any(|a| a.covers.contains(&at)) {
                failures.push(format!(
                    "{rel_str}:{}: calls a banned fs/process constructor (`{tok}`) outside any \
                     tagged allow.\n    → route it through a reviewed syscall site, or give the \
                     item an `#[allow(clippy::disallowed_methods, reason = \"[silent:<slug>] …\")]` \
                     and a row in SYSCALL_SITES.",
                    line_of(&text, at)
                ));
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

    let unaccounted: Vec<_> = found.difference(&manifest).collect();
    let stale: Vec<_> = manifest.difference(&found).collect();

    for (f, t) in unaccounted {
        failures.push(format!(
            "NEW unaccounted syscall site {f}  [{t}]\n    → review it, then add the pair to SYSCALL_SITES in this file."
        ));
    }
    for (f, t) in stale {
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
fn single_door_tokens_appear_only_at_their_door() {
    let root = workspace_root();
    let mut failures = Vec::new();
    let mut doors_seen = BTreeSet::new();

    for rel in production_files(&root) {
        let rel_str = rel.to_string_lossy().to_string();
        let text = std::fs::read_to_string(root.join(&rel))
            .unwrap_or_else(|e| panic!("read {rel_str}: {e}"));
        let code = blanked(&text);

        for (token, door) in SINGLE_DOORS {
            if code.contains(token) {
                if rel_str == *door {
                    doors_seen.insert(token);
                } else {
                    failures.push(format!(
                        "{rel_str}: calls `{token}`, whose one door is {door}\n    → route it through the door."
                    ));
                }
            }
        }
    }

    for (token, door) in SINGLE_DOORS {
        if !doors_seen.contains(token) {
            failures.push(format!(
                "{door}: no longer contains `{token}` — update SINGLE_DOORS."
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "single-door invariant violated ({} issue(s)):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn the_string_containment_helpers_gain_no_caller() {
    let root = workspace_root();
    let mut counted: BTreeMap<(String, &str), usize> = BTreeMap::new();

    for rel in production_files(&root) {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.starts_with("core/src/path/") || rel_str == "core/src/path.rs" {
            continue;
        }
        let text = std::fs::read_to_string(root.join(&rel))
            .unwrap_or_else(|e| panic!("read {rel_str}: {e}"));
        let code = blanked(&text);
        let names_rendered = code.contains("Rendered");
        for name in STRING_CONTAINMENT_HELPERS
            .iter()
            .chain(names_rendered.then_some(&".holds("))
        {
            let n = code.matches(name).count();
            if n > 0 {
                counted.insert((rel_str.clone(), name), n);
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
