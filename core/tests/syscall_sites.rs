#![allow(clippy::disallowed_methods)]

//! The closed syscall-site set (ADR `260619_surface-reads-writes-execs`,
//! Enforcement §2).
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
//! It walks the production `src/` of every workspace crate (plus the
//! `build.rs` files) — never the `tests/` trees, which carry blanket
//! `#![allow(clippy::disallowed_methods)]` for test scaffolding, a separate
//! category that is not a syscall site — and enforces three facts:
//!
//!   1. **Every tagged allow is well-formed.**  An allow whose `reason` begins
//!      with a `[…]` tag must name a known `<kind>` (`surface`, `silent`, or
//!      `test`) and carry a non-empty explanation after the tag — so a site is
//!      always a written decision, never an empty marker.  (Pre-existing bare
//!      `#[allow(clippy::disallowed_methods)]` for the *other* disciplines —
//!      path-construction, cwd, child-wait, env — are governed by their own
//!      denylist entries and are out of scope here; this test concerns the
//!      fs/process syscall sites.)
//!   2. **The site set equals the reviewed manifest.**  The set of
//!      `(crate-relative file, tag)` pairs for `surface`/`silent` sites must
//!      equal [`SYSCALL_SITES`], and each pair must occur *exactly once*.  A
//!      new site (or a removed/renamed one) fails until a human updates the
//!      manifest — the review gate.  Uniqueness is what keeps that gate shut:
//!      the manifest is keyed by pair, so a site reusing a slug its file
//!      already carries would land with no manifest diff at all.
//!   3. **A file that calls a banned constructor declares a site.**  If a
//!      production file invokes any banned fs/process constructor token but
//!      carries no tagged allow at all, it fails.  This is the closure: a new
//!      fs/process site added with a *bare* (untagged) allow satisfies clippy
//!      but is caught here, forcing it to be tagged and (via fact 2) reviewed.
//!      It also catches platform-`cfg`-gated sites a single-target clippy run
//!      on one OS would miss.
//!
//! ## Updating the manifest when you add a legitimate new site
//!
//! 1. Add the call site and its `#[allow(clippy::disallowed_methods, reason =
//!    "[surface:<slug>] …")]` (or `[silent:<slug>]`), with a `<slug>` unique
//!    within its file — enforced, not merely asked for.  Twins split by
//!    platform take the base slug and a suffix (`…-windows`, `…-linux`), so
//!    the `cfg` a site lives under is legible in the manifest.
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
    ("core/src/capability/load.rs", "silent:cap-load"),
    ("core/src/boot.rs", "silent:prelude-bake"),
    ("core/src/hatch.rs", "silent:hatch-spawn"),
    ("core/src/host.rs", "silent:date-launch"),
    ("core/src/host.rs", "silent:git-launch"),
    // The fs sites: `path/walk.rs` is the one file that opens a model-named
    // object, so every site below is a step of the locate-then-open recipe.
    ("core/src/path/walk.rs", "silent:discard-device"),
    ("core/src/path/walk.rs", "silent:locate-abandon"),
    ("core/src/path/walk.rs", "silent:locate-access"),
    ("core/src/path/walk.rs", "silent:locate-read-dir"),
    ("core/src/path/walk.rs", "silent:locate-read-link"),
    ("core/src/path/walk.rs", "silent:locate-stat"),
    ("core/src/path/walk.rs", "silent:walk-descend"),
    ("core/src/path/walk.rs", "silent:walk-link-probe"),
    ("core/src/path/walk.rs", "silent:walk-link-read"),
    ("core/src/path/walk.rs", "silent:walk-search"),
    ("core/src/path/walk.rs", "surface:locate-commit"),
    ("core/src/path/walk.rs", "surface:locate-open"),
    ("core/src/path/walk.rs", "surface:locate-stage"),
    ("core/src/path/walk.rs", "surface:locate-staged-read"),
    ("core/src/path/walk.rs", "surface:locate-staged-write"),
    ("core/src/path/which.rs", "silent:which-readdir"),
    ("core/src/path/which.rs", "silent:which-stat"),
    ("core/src/path/which.rs", "silent:which-stat-absent"),
    ("core/src/process/jail/linux.rs", "silent:jail-cgroup-procs"),
    ("core/src/process/jail/linux.rs", "silent:jail-cgroup-write"),
    ("core/src/process/jail/linux.rs", "silent:jail-seq-file"),
    ("core/src/process/launch.rs", "surface:process-launch"),
    ("core/src/process/spawn_lock.rs", "silent:cloexec-pipe"),
    (
        "core/src/process/spawn_lock.rs",
        "silent:cloexec-socketpair",
    ),
    ("core/src/process/spawn_lock.rs", "silent:spawn-door"),
    (
        "core/src/runtime/pipeline/helper.rs",
        "silent:self-reexec-windows",
    ),
    ("core/src/sandbox.rs", "silent:restricted-envelope-probe"),
    ("core/src/sandbox.rs", "silent:pin-identity"),
    ("core/src/sandbox.rs", "silent:self-reexec"),
    ("core/src/sandbox/diag.rs", "silent:ps-sample"),
    ("core/src/sandbox/diag/linux.rs", "silent:journal-read"),
    ("core/src/sandbox/diag/macos.rs", "silent:log-show"),
    ("core/src/sandbox/launch.rs", "silent:respawn-exec"),
    ("core/src/sandbox/launch.rs", "silent:respawn-spawn"),
    ("core/src/sandbox/linux.rs", "silent:own-cgroup"),
    ("core/src/sandbox/linux.rs", "surface:bwrap-launch"),
    ("core/src/sandbox/linux/host.rs", "silent:bwrap-host-probe"),
    ("core/src/sandbox/reexec.rs", "silent:pin-locate"),
    ("core/src/sandbox/reexec.rs", "silent:pin-open"),
    ("core/src/sandbox/reexec.rs", "silent:pin-stat"),
    ("core/src/sandbox/reexec.rs", "silent:pinned-exec"),
    ("core/src/sandbox/reexec.rs", "silent:verify-stat"),
    ("core/src/sandbox/windows/dacl.rs", "silent:dacl-apply"),
    (
        "core/src/sandbox/windows/dacl.rs",
        "silent:dacl-ledger-read",
    ),
    (
        "core/src/sandbox/windows/dacl.rs",
        "silent:dacl-ledger-remove",
    ),
    (
        "core/src/sandbox/windows/dacl.rs",
        "silent:dacl-ledger-sweep",
    ),
    (
        "core/src/sandbox/windows/dacl.rs",
        "silent:dacl-ledger-write",
    ),
    ("core/src/sandbox/windows/dacl.rs", "silent:dacl-stamp-read"),
    ("core/src/sandbox/windows/dacl.rs", "silent:dacl-state-dir"),
    (
        "core/src/sandbox/windows/dacl.rs",
        "silent:dacl-state-dir-recheck",
    ),
    ("core/src/subprocess_codec.rs", "silent:frame-dump"),
    ("core/src/subprocess_codec.rs", "silent:frame-dump-nonunix"),
    ("core/src/protocol.rs", "silent:engine-spawn"),
    ("core/src/types/shell/cwd.rs", "silent:cwd-stat"),
    ("core/src/uutils.rs", "silent:diff-read"),
    ("core/src/wire.rs", "silent:wire-pair-windows"),
    // ── guest-net ─────────────────────────────────────────────────────────
    ("guest-net/src/vet.rs", "silent:vet-dial"),
    ("guest-net/src/vet.rs", "silent:vet-resolve"),
    // ── exarch ────────────────────────────────────────────────────────────
    ("exarch/src/shell_eval/builtins.rs", "surface:grep-walk"),
    (
        "exarch/src/shell_eval/builtins/fff_index.rs",
        "silent:fff-db-dir",
    ),
    ("exarch/src/bootstrap.rs", "silent:log-run-dir"),
    ("exarch/src/bootstrap.rs", "silent:scratch-bootstrap"),
    ("exarch/src/bootstrap.rs", "silent:scratch-reap"),
    ("exarch/src/cli.rs", "silent:seed-file"),
    ("exarch/src/config.rs", "silent:config-load"),
    ("exarch/src/egress.rs", "silent:net-audit-open"),
    ("exarch/src/egress.rs", "silent:net-audit-rotate"),
    ("exarch/src/egress.rs", "silent:net-audit-size"),
    ("exarch/src/agent/event.rs", "silent:record-file"),
    ("exarch/src/agent/event.rs", "silent:session-dir"),
    ("exarch/src/record/log.rs", "silent:record-file-append"),
    ("exarch/src/record/log.rs", "silent:record-file-create"),
    ("exarch/src/record/log.rs", "silent:record-file-read"),
    ("exarch/src/record/log.rs", "silent:record-file-rotate"),
    ("exarch/src/record/log.rs", "silent:record-readable-mirror"),
    (
        "exarch/src/record/model/resume.rs",
        "silent:model-fold-crash-quarantine",
    ),
    (
        "exarch/src/record/model/resume.rs",
        "silent:model-fold-crash-scan",
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
    // fallback file share; moved here out of `oauth`, where the two sites
    // above it used to live.
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
    ("core/src/path/git.rs", "silent:git-dir-backpointer"),
    ("core/src/path/git.rs", "silent:git-dir-config-worktree"),
    ("core/src/path/git.rs", "silent:git-dir-discovery"),
    ("core/src/path/git.rs", "silent:git-dir-pointer"),
    ("core/src/path/lex.rs", "silent:mount-shape"),
    ("exarch/src/prompt.rs", "silent:system-prompt-files"),
    ("exarch/src/shell_eval/skill.rs", "silent:skill-list-dir"),
    ("exarch/src/shell_eval/skill.rs", "silent:skill-metadata"),
    ("exarch/src/shell_eval/skill.rs", "surface:skill-body"),
    ("exarch/src/shell_eval/skill.rs", "surface:skill-list"),
    ("exarch/src/provider/state.rs", "silent:state-read"),
    ("exarch/src/provider/state.rs", "silent:state-write"),
    (
        "exarch/src/agent/resources.rs",
        "silent:resources-disk-probe",
    ),
    ("exarch/src/tui/terminal.rs", "silent:editor-compose"),
    ("exarch/src/tui/terminal.rs", "silent:stderr-log"),
    ("exarch/src/tui/terminal.rs", "silent:stderr-log-windows"),
    ("exarch/src/tui/scrollback.rs", "silent:export"),
    ("exarch/src/tui/scrollback.rs", "silent:scrollback-log"),
    // ── ral / ral-sh ──────────────────────────────────────────────────────
    ("ral-sh/src/main.rs", "silent:respawn-posix-sh"),
    ("ral-sh/src/main.rs", "silent:respawn-ral"),
    ("ral/build.rs", "silent:git-probe"),
    ("ral/src/batch.rs", "silent:script-read"),
    ("ral/src/platform.rs", "silent:exit-hints-read"),
    ("ral/src/repl/completion.rs", "silent:complete-readdir"),
    ("ral/src/repl/config.rs", "silent:history-mkdir"),
    ("ral/src/repl/config.rs", "silent:rc-write"),
    ("ral/src/repl/frontend.rs", "silent:history-append"),
    ("ral/src/repl/frontend.rs", "silent:history-read"),
    ("ral/src/repl/plugin/load.rs", "silent:plugin-read"),
    ("ral/src/repl/session/boot.rs", "silent:config-read"),
    ("ral/src/repl/session/boot.rs", "silent:crashlog-write"),
];

/// Production source roots, crate-relative to the workspace root.  The
/// vendored `ral-ripgrep-core` opts out of the workspace clippy lints (it
/// stays upstream-diffable) so it is *not* governed by this discipline and is
/// excluded here.  `guest-net` is here because the CONNECT proxy is on the
/// model's own egress path; the host-side crates (`synod`, `vm-manager`) are
/// not yet.
const SRC_ROOTS: &[&str] = &[
    "core/src",
    "exarch/src",
    "guest-net/src",
    "ral/src",
    "ral-sh/src",
];

/// Build scripts are production code too, and `ral/build.rs` spawns `git`.
const BUILD_SCRIPTS: &[&str] = &["core/build.rs", "ral/build.rs", "exarch/build.rs"];

/// Banned fs/process/network constructor tokens (substring match).  Coarse on
/// purpose: fact (3) only needs "this file makes such a call", not clippy's
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
    "Command::new",
    "CommandExt::exec",
    // The pipe door: `cloexec_pipe` closes a CLOEXEC window Apple leaves open
    // between `pipe()` and the flag, so a raw `os_pipe::pipe()` is a site too.
    // Listed here rather than left to clippy because the one that got through
    // was `cfg(windows)`, which a Unix clippy run never compiles.
    "os_pipe::pipe(",
    // The cap-std twins are imported by name and then called bare, so the
    // import is the token that betrays them.
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

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `…/core` at compile time; the workspace root is
    // its parent.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core/ has a parent (the workspace root)")
        .to_path_buf()
}

/// Recursively collect every `.rs` file under `dir`.
fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
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

/// Every occurrence of `#[allow(clippy::disallowed_methods …)]` in `text`,
/// returned as the raw text from after `disallowed_methods` up to the closing
/// `)]` of the attribute (so a multi-line attribute with a `reason = "…"` is
/// captured whole).
fn allow_attrs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = "clippy::disallowed_methods";
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find(needle) {
        let start = search_from + rel;
        // Find the end of the enclosing attribute: the next `)]` after `start`.
        let end = text[start..]
            .find(")]")
            .map_or(text.len(), |e| start + e + 2);
        out.push(text[start..end].to_string());
        search_from = end;
    }
    out
}

/// The file's source with line comments stripped, so a banned constructor
/// *named in a doc comment* (e.g. "`Command::new`'d directly") is not mistaken
/// for a call site.  Block comments are rare in this tree and a banned token
/// inside one is harmless to flag, so only line/doc comments are removed.
fn strip_line_comments(text: &str) -> String {
    text.lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the `reason = "…"` payload from one attribute slice, if present.
fn reason_of(attr: &str) -> Option<String> {
    let after = attr.find("reason")?;
    let eq = attr[after..].find('=')? + after;
    let q1 = attr[eq..].find('"')? + eq + 1;
    let q2 = attr[q1..].find('"')? + q1;
    Some(attr[q1..q2].to_string())
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

        let attrs = allow_attrs(&text);

        // Fact (1): every tagged allow is well-formed (known kind, non-empty
        // explanation).  Untagged allows belong to the other
        // disallowed-methods disciplines and are not this test's concern;
        // fact (3) below is what stops an fs/process site hiding behind one.
        for attr in &attrs {
            let Some(tag) = reason_of(attr).as_deref().and_then(parse_tag) else {
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
        }

        // Fact (3): a file that calls a banned constructor must declare a
        // syscall site (any `[…]` allow — surface, silent, or test — counts; a
        // test-only file uses the test tag, a production site uses surface /
        // silent).
        let has_any_tag = attrs
            .iter()
            .filter_map(|a| reason_of(a))
            .any(|r| parse_tag(&r).is_some());
        if !has_any_tag {
            let code = strip_line_comments(&text);
            if let Some(tok) = BANNED_TOKENS.iter().find(|t| code.contains(**t)) {
                failures.push(format!(
                    "{rel_str}: calls a banned fs/process constructor (`{tok}`) but declares no \
                     tagged allow. Route it through a reviewed syscall site or add a tagged allow."
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
