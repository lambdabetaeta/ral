#![allow(clippy::disallowed_methods)]

//! The closed I/O-door set (ADR `260619_surface-reads-writes-execs`,
//! Enforcement §2).
//!
//! The clippy `disallowed-methods` denylist (see `clippy.toml`) bans every
//! filesystem and process *constructor* outside a handful of known doors, so
//! the only way to read, write, spawn, or exec is through a door that either
//! *surfaces* the operation as a card or is a *reasoned-silent* site.  Each
//! door carries an `#[allow(clippy::disallowed_methods, reason = "[io-door:…] …")]`
//! whose tag records which kind it is.  Clippy guarantees there is no I/O
//! *outside* a door; this meta-test guarantees the door set is *closed*: no
//! new, unaccounted door can slip in unreviewed.
//!
//! It walks the production `src/` of every workspace crate (plus the
//! `build.rs` files) — never the `tests/` trees, which carry blanket
//! `#![allow(clippy::disallowed_methods)]` for test scaffolding, a separate
//! category that is not a door — and enforces three facts:
//!
//!   1. **Every I/O-door allow is well-formed.**  An allow whose `reason`
//!      begins with `[io-door:…]` must name a known `<kind>` (`surface`,
//!      `silent`, or `test`) and carry a non-empty explanation after the tag —
//!      so a door is always a written decision, never an empty marker.
//!      (Pre-existing bare `#[allow(clippy::disallowed_methods)]` for the
//!      *other* disciplines — path-construction, cwd, child-wait, env — are
//!      governed by their own denylist entries and are out of scope here; this
//!      test concerns the fs/process *I/O* doors.)
//!   2. **The door set equals the reviewed manifest.**  The set of
//!      `(crate-relative file, tag)` pairs for `surface`/`silent` doors must
//!      equal [`DOOR_MANIFEST`].  A new door (or a removed/renamed one) fails
//!      until a human updates the manifest — the review gate.
//!   3. **A file that calls a banned constructor declares a door.**  If a
//!      production file invokes any banned fs/process constructor token but
//!      carries no `[io-door:…]` allow at all, it fails.  This is the closure:
//!      a new fs/process door added with a *bare* (untagged) allow satisfies
//!      clippy but is caught here, forcing it to be tagged and (via fact 2)
//!      reviewed.  It also catches platform-`cfg`-gated sites a single-target
//!      clippy run on one OS would miss.
//!
//! ## Updating the manifest when you add a legitimate new door
//!
//! 1. Add the call site and its `#[allow(clippy::disallowed_methods, reason =
//!    "[io-door:surface:<slug>] …")]` (or `:silent:`), with a `<slug>` unique
//!    within its file.
//! 2. Add the `(file, "io-door:<kind>:<slug>")` pair to [`DOOR_MANIFEST`]
//!    below, in the right crate block.
//! 3. This is the review gate: the diff to `DOOR_MANIFEST` is where a reviewer
//!    confirms the new door is genuinely a door (and surfacing/silent as
//!    claimed), not an accidental ungated open.
//!
//! Test-mod allows (`[io-door:test]`, on a `#[cfg(test)] mod tests`) are *not*
//! manifest entries — test fs/process use is blanket-allowed — but they must
//! still carry the tag so fact (1) holds uniformly.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The reviewed door set: `(crate-relative path, tag)` for every `surface` and
/// `silent` I/O door in production source.  Tags are stable across line shifts
/// — keyed by file + slug, never line number — so editing a door file does not
/// flake the test; only adding/removing/renaming a door does.
///
/// Keep this sorted by path for review-friendly diffs.
const DOOR_MANIFEST: &[(&str, &str)] = &[
    // ── core ──────────────────────────────────────────────────────────────
    ("core/src/builtins/fs.rs", "io-door:silent:file-info"),
    ("core/src/builtins/fs.rs", "io-door:silent:list-dir"),
    ("core/src/builtins/fs.rs", "io-door:silent:stat-follow"),
    ("core/src/builtins/fs.rs", "io-door:silent:stat-nofollow"),
    (
        "core/src/builtins/fs.rs",
        "io-door:silent:writable-stat-nonunix",
    ),
    ("core/src/builtins/misc.rs", "io-door:silent:ask-tty"),
    ("core/src/builtins/modules.rs", "io-door:silent:module-load"),
    ("core/src/builtins/uutils.rs", "io-door:silent:diff-read"),
    ("core/build.rs", "io-door:silent:prelude-bake-build"),
    ("core/src/capability/load.rs", "io-door:silent:cap-load"),
    ("core/src/driver.rs", "io-door:silent:prelude-bake"),
    ("core/src/host.rs", "io-door:silent:date-launch"),
    ("core/src/host.rs", "io-door:silent:git-launch"),
    ("core/src/path/which.rs", "io-door:silent:which-readdir"),
    ("core/src/path/which.rs", "io-door:silent:which-stat"),
    (
        "core/src/process/launch.rs",
        "io-door:surface:process-launch",
    ),
    (
        "core/src/runtime/command/redirect.rs",
        "io-door:surface:atomic-commit",
    ),
    (
        "core/src/runtime/command/redirect.rs",
        "io-door:surface:atomic-eligible",
    ),
    (
        "core/src/runtime/command/redirect.rs",
        "io-door:surface:atomic-old-read",
    ),
    (
        "core/src/runtime/command/redirect.rs",
        "io-door:surface:atomic-temp-read",
    ),
    (
        "core/src/runtime/command/redirect.rs",
        "io-door:surface:open-atomic",
    ),
    (
        "core/src/runtime/command/redirect.rs",
        "io-door:surface:open-file",
    ),
    (
        "core/src/runtime/command/redirect.rs",
        "io-door:surface:stdin-redirect",
    ),
    (
        "core/src/runtime/command/uutils.rs",
        "io-door:silent:uutils-cwd-restore",
    ),
    (
        "core/src/runtime/command/uutils.rs",
        "io-door:silent:uutils-cwd-save",
    ),
    (
        "core/src/runtime/pipeline/helper.rs",
        "io-door:silent:self-reexec-windows",
    ),
    ("core/src/sandbox.rs", "io-door:silent:self-reexec"),
    ("core/src/sandbox.rs", "io-door:surface:make-command"),
    ("core/src/sandbox/diag.rs", "io-door:silent:ps-sample"),
    (
        "core/src/sandbox/diag/linux.rs",
        "io-door:silent:journal-read",
    ),
    ("core/src/sandbox/diag/macos.rs", "io-door:silent:log-show"),
    ("core/src/sandbox/launch.rs", "io-door:silent:respawn-exec"),
    ("core/src/sandbox/launch.rs", "io-door:silent:respawn-spawn"),
    ("core/src/sandbox/linux.rs", "io-door:surface:bwrap-launch"),
    ("core/src/sandbox/reexec.rs", "io-door:silent:pin-open"),
    ("core/src/sandbox/reexec.rs", "io-door:silent:pin-stat"),
    ("core/src/sandbox/reexec.rs", "io-door:silent:self-reexec"),
    ("core/src/sandbox/reexec.rs", "io-door:silent:verify-stat"),
    ("core/src/sandbox/windows/dacl.rs", "io-door:silent:dacl-apply"),
    (
        "core/src/sandbox/windows/dacl.rs",
        "io-door:silent:dacl-ledger-read",
    ),
    (
        "core/src/sandbox/windows/dacl.rs",
        "io-door:silent:dacl-ledger-remove",
    ),
    (
        "core/src/sandbox/windows/dacl.rs",
        "io-door:silent:dacl-ledger-sweep",
    ),
    (
        "core/src/sandbox/windows/dacl.rs",
        "io-door:silent:dacl-ledger-write",
    ),
    (
        "core/src/sandbox/windows/dacl.rs",
        "io-door:silent:dacl-state-dir",
    ),
    ("core/src/subprocess_codec.rs", "io-door:silent:frame-dump"),
    (
        "core/src/subprocess_codec.rs",
        "io-door:silent:frame-dump-nonunix",
    ),
    ("core/src/transport.rs", "io-door:silent:engine-spawn"),
    ("core/src/types/shell/cwd.rs", "io-door:silent:cwd-stat"),
    // ── exarch ────────────────────────────────────────────────────────────
    (
        "exarch/src/agent_builtins.rs",
        "io-door:surface:witness-read",
    ),
    ("exarch/src/agent_builtins.rs", "io-door:surface:grep-read"),
    ("exarch/src/agent_builtins.rs", "io-door:surface:grep-walk"),
    (
        "exarch/src/agent_builtins/fff_index.rs",
        "io-door:silent:fff-db-dir",
    ),
    ("exarch/src/bootstrap.rs", "io-door:silent:log-run-dir"),
    (
        "exarch/src/bootstrap.rs",
        "io-door:silent:scratch-bootstrap",
    ),
    ("exarch/src/cli.rs", "io-door:silent:seed-file"),
    ("exarch/src/config.rs", "io-door:silent:provider-config"),
    ("exarch/src/event.rs", "io-door:silent:events-file"),
    ("exarch/src/event.rs", "io-door:silent:session-dir"),
    ("exarch/src/models.rs", "io-door:silent:models-cache-read"),
    ("exarch/src/models.rs", "io-door:silent:models-cache-write"),
    (
        "exarch/src/oauth/browser.rs",
        "io-door:silent:browser-launch",
    ),
    (
        "exarch/src/oauth/browser.rs",
        "io-door:silent:browser-launch-linux",
    ),
    ("exarch/src/oauth/mod.rs", "io-door:silent:token-dir"),
    ("exarch/src/oauth/mod.rs", "io-door:silent:token-read"),
    ("exarch/src/oauth/mod.rs", "io-door:silent:token-remove"),
    ("exarch/src/oauth/mod.rs", "io-door:silent:token-write"),
    (
        "exarch/src/oauth/mod.rs",
        "io-door:silent:token-write-nonunix",
    ),
    ("core/src/path/git.rs", "io-door:silent:git-dir-discovery"),
    ("exarch/src/prompt.rs", "io-door:silent:system-prompt-files"),
    ("exarch/src/skill.rs", "io-door:silent:skill-list-dir"),
    ("exarch/src/skill.rs", "io-door:silent:skill-metadata"),
    ("exarch/src/skill.rs", "io-door:surface:skill-body"),
    ("exarch/src/skill.rs", "io-door:surface:skill-list"),
    ("exarch/src/state.rs", "io-door:silent:state-read"),
    ("exarch/src/state.rs", "io-door:silent:state-write"),
    ("exarch/src/resources.rs", "io-door:silent:resources-disk-probe"),
    ("exarch/src/transcript.rs", "io-door:silent:transcript-file"),
    (
        "exarch/src/tui/terminal.rs",
        "io-door:silent:editor-compose",
    ),
    ("exarch/src/tui/terminal.rs", "io-door:silent:stderr-log"),
    ("exarch/src/tui/viewport.rs", "io-door:silent:export"),
    ("exarch/src/tui/viewport.rs", "io-door:silent:viewport-log"),
    // ── ral / ral-sh ──────────────────────────────────────────────────────
    ("ral-sh/src/main.rs", "io-door:silent:respawn-posix-sh"),
    ("ral-sh/src/main.rs", "io-door:silent:respawn-ral"),
    ("ral/build.rs", "io-door:silent:git-probe"),
    ("ral/src/main.rs", "io-door:silent:script-read"),
    ("ral/src/platform.rs", "io-door:silent:exit-hints-read"),
    (
        "ral/src/repl/completion.rs",
        "io-door:silent:complete-readdir",
    ),
    ("ral/src/repl/config.rs", "io-door:silent:history-mkdir"),
    ("ral/src/repl/config.rs", "io-door:silent:rc-write"),
    ("ral/src/repl/frontend.rs", "io-door:silent:history-append"),
    ("ral/src/repl/frontend.rs", "io-door:silent:history-read"),
    ("ral/src/repl/plugin/load.rs", "io-door:silent:plugin-read"),
    ("ral/src/repl/session/boot.rs", "io-door:silent:config-read"),
    (
        "ral/src/repl/session/boot.rs",
        "io-door:silent:crashlog-write",
    ),
];

/// Production source roots, crate-relative to the workspace root.  The
/// vendored `ral-ripgrep-core` opts out of the workspace clippy lints (it
/// stays upstream-diffable) so it is *not* governed by the door discipline and
/// is excluded here.
const SRC_ROOTS: &[&str] = &["core/src", "exarch/src", "ral/src", "ral-sh/src"];

/// Build scripts are production code too, and `ral/build.rs` spawns `git`.
const BUILD_SCRIPTS: &[&str] = &["core/build.rs", "ral/build.rs", "exarch/build.rs"];

/// Banned fs/process constructor tokens (substring match).  Coarse on purpose:
/// fact (3) only needs "this file touches a door", not clippy's resolution.
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

/// A parsed `[io-door:<kind>[:<slug>]]` tag plus the explanation after it.
struct Tag {
    kind: String,
    /// Full `io-door:<kind>:<slug>` token (without brackets); `io-door:test`
    /// for the test kind.
    full: String,
    explanation: String,
}

/// Parse the leading `[io-door:…]` tag out of an allow `reason` string.
fn parse_tag(reason: &str) -> Option<Tag> {
    let reason = reason.trim_start();
    let rest = reason.strip_prefix("[io-door:")?;
    let close = rest.find(']')?;
    let body = &rest[..close]; // e.g. "silent:which-stat" or "test"
    let explanation = rest[close + 1..].trim().to_string();
    let kind = body.split(':').next().unwrap_or("").to_string();
    Some(Tag {
        kind,
        full: format!("io-door:{body}"),
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
fn every_production_disallowed_allow_is_a_tagged_door() {
    let root = workspace_root();
    let mut failures = Vec::new();
    let mut found_doors: BTreeSet<(String, String)> = BTreeSet::new();

    for rel in production_files(&root) {
        let rel_str = rel.to_string_lossy().to_string();
        let text = std::fs::read_to_string(root.join(&rel))
            .unwrap_or_else(|e| panic!("read {rel_str}: {e}"));

        let attrs = allow_attrs(&text);

        // Fact (1): every io-door-tagged allow is well-formed (known kind,
        // non-empty explanation).  Untagged allows belong to the other
        // disallowed-methods disciplines and are not this test's concern;
        // fact (3) below is what stops an fs/process door hiding behind one.
        for attr in &attrs {
            let Some(tag) = reason_of(attr).as_deref().and_then(parse_tag) else {
                continue;
            };
            if !matches!(tag.kind.as_str(), "surface" | "silent" | "test") {
                failures.push(format!(
                    "{rel_str}: unknown io-door kind `{}` (expected surface|silent|test) in `{}`.",
                    tag.kind, tag.full
                ));
            }
            if tag.explanation.is_empty() {
                failures.push(format!(
                    "{rel_str}: io-door tag `{}` has an empty explanation after it.",
                    tag.full
                ));
            }
            if tag.kind == "surface" || tag.kind == "silent" {
                found_doors.insert((rel_str.clone(), tag.full));
            }
        }

        // Fact (3): a file that calls a banned constructor must declare a door
        // (any `[io-door:…]` allow — surface, silent, or test — counts; a
        // test-only file uses the test tag, a production door uses surface /
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
                     `[io-door:…]` allow. Route it through a door or add a tagged allow."
                ));
            }
        }
    }

    // Fact (2): the surface/silent door set equals the reviewed manifest.
    let manifest: BTreeSet<(String, String)> = DOOR_MANIFEST
        .iter()
        .map(|(f, t)| (f.to_string(), t.to_string()))
        .collect();

    let unaccounted: Vec<_> = found_doors.difference(&manifest).collect();
    let stale: Vec<_> = manifest.difference(&found_doors).collect();

    for (f, t) in unaccounted {
        failures.push(format!(
            "NEW unaccounted door {f}  [{t}]\n    → review it, then add the pair to DOOR_MANIFEST in this file."
        ));
    }
    for (f, t) in stale {
        failures.push(format!(
            "STALE manifest entry {f}  [{t}] no longer exists in source\n    → remove it from DOOR_MANIFEST."
        ));
    }

    assert!(
        failures.is_empty(),
        "I/O-door invariant violated ({} issue(s)):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
