#![allow(clippy::disallowed_methods, reason = "test code")]

//! What clippy cannot say about its own bans: which exemptions exist, whether a
//! lint group silences them, and whether every member is bound by them.  And the
//! caller set that stays pinned (ADR `261022_typed-sites`, ADR
//! `261021_one-path`, ADR `261022_own-files`).
//!
//! `clippy.toml` bans every filesystem, process, network and lock constructor,
//! and the raw wrappers the walk is made of, under the one lint
//! `clippy::disallowed_methods`, which the workspace denies.  Clippy resolves
//! the names, aliases and imports included, and CI runs it on Linux, macOS and
//! Windows, so no call escapes a ban on any platform's `cfg`.  A legitimate
//! call is exempt only as a typed site (`lint/site.rs`): `site!` wraps exactly
//! one call, its first argument is a variant of the crate's `Surface` or
//! `Silent` enum, which names the place and says why it is exempt, and the
//! exemption it emits is an `#[expect]`, so a site that wraps no banned call is
//! an error.  The enums are the manifest, and `dead_code` reports a variant no
//! site uses; nothing here keeps a list of them.
//!
//! What clippy cannot say is that no *other* exemption exists: the lint is one
//! lint for every ban, and a hand-written `allow` over any call silences it.
//! [`no_other_exemption_exists`] scans the Rust sources of every workspace
//! member but `vendor/`, with whole-line comments dropped, and every remaining
//! occurrence of the lint's name, its old spelling included, must be one of four.
//!
//!   (a) In `lint/site.rs`, which emits the `#[expect]` of a site.  The file is
//!       not a workspace member, so it is not scanned.
//!   (b) An inner `#![allow(…, reason = "…")]` among the leading attributes of
//!       a file that is a whole unit of non-production code (only other inner
//!       attributes precede it): an integration-test crate root
//!       (`<crate>/tests/*.rs`), an example (`<crate>/examples/*.rs`), a
//!       `build.rs`, or the crate root of `ral-guest`,
//!       a guest init program whose whole job is the syscalls the bans
//!       route and which links no ral shell.  The modules such a root includes
//!       carry none of their own.
//!   (c) An outer `#[allow(…, reason = "test code")]` on a `mod` item whose own
//!       attributes include a `#[cfg(…)]` whose predicate implies `test`:
//!       exactly `test`, an `all(…)` with a `test` conjunct, or exactly
//!       `any(test, feature = "test-util")`.  Any other `any(…)` holds in a
//!       production build (`any(test, unix)` does), so it exempts production
//!       code.  The blanket is on the declaration, and nothing inside the module
//!       carries one.
//!   (d) The one third-party-codegen exception: the `#[expect(…)]` on the
//!       `tauri::generate_context!()` statement of `synod/src/main.rs`, whose
//!       expansion exits the process on a panic, and `site!` wraps calls, not
//!       macro invocations.
//!
//! A failure names `file:line` and the rule the occurrence breaks.  The scan
//! never spells either name of the lint in code, only in the attribute above and
//! in comments, so that it does not flag itself.
//!
//! Two more facts close the ways round the lint's own name.
//!
//!   - **No group silences the lint.**  `disallowed_methods` belongs to clippy's
//!     `style` group, which `clippy::all` holds, so an `allow`, `expect` or
//!     `warn` of either group would silence every ban without naming the lint.
//!     [`no_group_silences_the_lint`] scans the same sources, and flags, with its
//!     `file:line`, every attribute that names either group and is not a `deny`
//!     or a `forbid`.
//!   - **Every member is bound.**  The bans and their `deny` live in the
//!     workspace's lints, which a member takes only through `[lints]` with
//!     `workspace = true` in its `Cargo.toml`.
//!     [`every_member_inherits_the_workspace_lints`] reads the manifest of every
//!     workspace member but `vendor/`'s, and fails the one that lacks it.
//!
//! One count stays pinned, since a ban cannot scope itself to a module or count
//! anything:
//!
//!   - **The string containment helpers gain no caller.**
//!     [`STRING_CONTAINMENT_CALLERS`] pins, outside `path/`, each file that
//!     calls `starts_with_identity`, `path_within_str`, `proper_ancestors` or
//!     `Rendered::{holds, rebased}`, and how often. Bwrap's envelope names can
//!     be surface spellings through links; Seatbelt's firmlink spellings have
//!     ancestors that are not renderings of a position's ancestors. These are
//!     lexical names, so the helpers stay and gain no caller.
//!
//! `authority::policy::Ungoverned`'s authority-minting methods are banned by clippy,
//! including aliases and function-item references. Legitimate reads and
//! `own`'s crate-private writes require documented typed sites, not text counts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The lint's name, in two pieces: this file's code never holds it whole, so
/// the scan has nothing of its own to find.
const LINT: &str = concat!("clippy::disallowed_", "methods");

/// The lint's old name, in two pieces like [`LINT`].  Clippy still honours it,
/// with a rename warning, so an `allow` of it silences the lint as the current
/// name does: the scan reads it as the lint.  It is also the stem of [`LINT`],
/// so a search for it finds both spellings.
const OLD_LINT: &str = concat!("clippy::disallowed_", "method");

/// The other lint the one third-party exception silences.
const MACROS_LINT: &str = "clippy::disallowed_macros";

/// The lint groups that hold the lint: `style`, where clippy files it, and
/// `all`, which holds `style`.  Spelled in pieces, like [`LINT`].
const LINT_GROUPS: [&str; 2] = [concat!("clippy::", "style"), concat!("clippy::", "all")];

/// The file that holds the one third-party-codegen exception, and the statement
/// it precedes.
const TAURI_FILE: &str = "synod/src/main.rs";
const TAURI_STATEMENT: &str = "let context = tauri::generate_context!(";

/// The crate roots, outside `tests/`, `examples/` and `build.rs`, that carry a
/// crate-wide blanket: guest init programs, outside the discipline.
const GUEST_INIT_ROOTS: &[&str] = &["ral-guest/src/main.rs"];

/// Production source roots, crate-relative to the workspace root.  The
/// vendored `ral-ripgrep-core` opts out of the workspace clippy lints (it
/// stays upstream-diffable) so it is *not* governed by this discipline.
/// `netgate` is here because the CONNECT proxy is on the model's own egress
/// path, and `synod` because its review hands a folder and a name to the OS;
/// `vm-manager` is a host tool whose names no grant judges.
const SRC_ROOTS: &[&str] = &[
    "core/src",
    "exarch/src",
    "netgate/src",
    "ral/src",
    "synod/src",
];

/// The build scripts.  Each carries a blanket under rule (b), since
/// `ral/build.rs` spawns `git`, so the bans do not govern them; they count as
/// production only for the pinned caller set below.
const BUILD_SCRIPTS: &[&str] = &["core/build.rs", "ral/build.rs", "exarch/build.rs"];

/// The string containment helpers, which `path/` exports for the sandbox
/// renderers that still hold names as strings: each file outside `path/` that
/// calls one, with the helper and how many times.  `Rendered::{holds, rebased}`
/// are counted as `.holds(` and `.rebased(` in a file that names `Rendered`.
/// Nothing new may join this list.
const STRING_CONTAINMENT_CALLERS: &[(&str, &str, usize)] = &[
    ("core/src/authority/sandbox/linux.rs", ".holds(", 2),
    ("core/src/authority/sandbox/linux.rs", ".rebased(", 1),
    ("core/src/authority/sandbox/macos.rs", "proper_ancestors", 2),
];

/// What `Rendered` lends a renderer, counted in a file that names it.
const RENDERED_NAME_HELPERS: &[&str] = &[".holds(", ".rebased("];

/// The helpers `STRING_CONTAINMENT_CALLERS` counts, by name.
const STRING_CONTAINMENT_HELPERS: &[&str] = &[
    "starts_with_identity",
    "path_within_str",
    "proper_ancestors",
];

// ── The sources ───────────────────────────────────────────────────────

/// A Rust source file of a workspace member.
struct Source {
    /// Relative to the workspace root, `/`-separated.
    rel: String,
    /// Relative to its member's directory, `/`-separated.
    within: String,
    /// Its text with every whole-line comment, doc comments among them, blanked
    /// (the line stays, so a byte's line number is the file's), so that a name
    /// a comment mentions is neither an exemption nor a caller.
    code: String,
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `…/core` at compile time; the workspace root is
    // its parent.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core/ has a parent (the workspace root)")
        .to_path_buf()
}

/// The workspace's members but `vendor/`'s, as the directories the root
/// manifest's `members` list names.
fn members(root: &Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .expect("the workspace manifest is readable");
    let list = manifest
        .split_once("members = [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .expect("the workspace manifest lists its members")
        .0;
    let members: Vec<String> = list
        .lines()
        .filter_map(|line| line.split('#').next()?.split('"').nth(1))
        .filter(|member| !member.starts_with("vendor/"))
        .map(str::to_owned)
        .collect();
    assert!(
        members.iter().any(|member| member == "core"),
        "the members list was not read: {members:?}"
    );
    members
}

/// Every `.rs` file under `dir`, but build output, dependencies and hidden
/// folders.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            let skipped = name == "target" || name == "node_modules";
            if !skipped && !name.to_string_lossy().starts_with('.') {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `text` with every whole-line comment blanked.
fn code_of(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every Rust source of every workspace member but `vendor/`.
fn workspace_sources() -> Vec<Source> {
    let root = workspace_root();
    let mut sources = Vec::new();
    for member in members(&root) {
        let mut files = Vec::new();
        rust_files(&root.join(&member), &mut files);
        files.sort();
        for path in files {
            let rel = path
                .strip_prefix(&root)
                .expect("file is under the workspace root")
                .to_string_lossy()
                .replace('\\', "/");
            let within = rel
                .strip_prefix(&format!("{member}/"))
                .expect("file is under its member")
                .to_owned();
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
            sources.push(Source {
                rel,
                within,
                code: code_of(&text),
            });
        }
    }
    sources
}

/// Whether `rel` is production code: below a production source root but not in
/// a unit-test module, which lives out of line as `tests.rs` or under a
/// `tests/` directory and is scaffolding; or a build script.
fn is_production(rel: &str) -> bool {
    BUILD_SCRIPTS.contains(&rel)
        || SRC_ROOTS.iter().any(|root| {
            rel.strip_prefix(root)
                .and_then(|below| below.strip_prefix('/'))
                .is_some_and(|below| !below.split('/').any(|p| p == "tests" || p == "tests.rs"))
        })
}

/// The production sources, the ones whose callers are pinned.
fn production_sources() -> Vec<Source> {
    workspace_sources()
        .into_iter()
        .filter(|source| is_production(&source.rel))
        .collect()
}

// ── The attributes that name the lint ─────────────────────────────────

/// An attribute that names the lint, as the scan reads it.
struct Attribute {
    /// Where its `#` and one past its closing `]` lie in the code.
    start: usize,
    end: usize,
    /// `#![…]` rather than `#[…]`.
    inner: bool,
    /// `allow` or `expect`.
    name: String,
    /// The lints it lists, in order.
    lints: Vec<String>,
    /// The text of its `reason`, where it gives one.
    reason: Option<String>,
}

/// One past the `]` that closes the attribute beginning at `start`: brackets
/// nest, and a string (a reason, say) is skipped whole.
fn attribute_end(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 0usize;
    let mut at = start;
    while at < bytes.len() {
        match bytes[at] {
            b'"' => {
                at += 1;
                while at < bytes.len() && bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
            }
            b'[' => depth += 1,
            b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// The text inside the first string literal of `rest`.
fn string_text(rest: &str) -> Option<String> {
    let rest = &rest[rest.find('"')? + 1..];
    let mut close = 0;
    while let Some(&byte) = rest.as_bytes().get(close)
        && byte != b'"'
    {
        close += if byte == b'\\' { 2 } else { 1 };
    }
    Some(rest[..close.min(rest.len())].to_owned())
}

/// The attribute that begins at `start`, as far as the scan reads one: its
/// name, the lints it lists, and its reason.
fn attribute_at(code: &str, start: usize) -> Option<Attribute> {
    let end = attribute_end(code, start)?;
    let text = &code[start..end];
    let body = &text[text.find('[')? + 1..text.len() - 1];
    let (name, args) = body.split_once('(')?;
    let args = &args[..args.rfind(')')?];
    let (lints, reason) = match args.find("reason") {
        Some(at) => (&args[..at], string_text(&args[at..])),
        None => (args, None),
    };
    Some(Attribute {
        start,
        end,
        inner: text.starts_with("#!"),
        name: name.trim().to_owned(),
        lints: lints
            .split(',')
            .map(str::trim)
            .filter(|lint| !lint.is_empty())
            .map(str::to_owned)
            .collect(),
        reason,
    })
}

/// The attribute whose lint list holds the name at byte `at`; `None` where the
/// name stands anywhere else.
fn enclosing_attribute(code: &str, at: usize) -> Option<Attribute> {
    let head = &code[..at];
    let start = head
        .rfind("#[")
        .into_iter()
        .chain(head.rfind("#!["))
        .max()?;
    let attribute = attribute_at(code, start)?;
    (attribute.end > at).then_some(attribute)
}

/// The one-line attributes that run up to `start` on the lines before it,
/// nearest first.
fn attributes_before(code: &str, start: usize) -> Vec<&str> {
    let mut found = Vec::new();
    let mut upto = code[..start].trim_end();
    while upto.ends_with(']') {
        let line_start = upto.rfind('\n').map_or(0, |i| i + 1);
        let line = upto[line_start..].trim_start();
        if !line.starts_with("#[") {
            break;
        }
        found.push(line);
        upto = upto[..line_start].trim_end();
    }
    found
}

/// The attributes that follow `from`, and then the head of the item or
/// statement they stand on, up to its `;` or `{`.
fn attributes_after(code: &str, from: usize) -> (Vec<&str>, &str) {
    let mut found = Vec::new();
    let mut at = from;
    loop {
        let rest = code[at..].trim_start();
        let begin = code.len() - rest.len();
        if !rest.starts_with("#[") {
            let head = rest.find([';', '{']).map_or(rest, |end| &rest[..end]);
            return (found, head.trim());
        }
        let Some(end) = attribute_end(code, begin) else {
            return (found, "");
        };
        found.push(&code[begin..end]);
        at = end;
    }
}

/// Whether `head` is the head of a `mod` item, whatever its visibility.
fn is_mod_item(head: &str) -> bool {
    let rest = head.strip_prefix("pub").map_or(head, str::trim_start);
    let rest = rest
        .strip_prefix('(')
        .and_then(|restricted| restricted.split_once(')'))
        .map_or(rest, |(_, after)| after.trim_start());
    rest.starts_with("mod ")
}

/// The arguments of `list`, split at its top-level commas: parentheses nest, and
/// a string is skipped whole.
fn top_level_arguments(list: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let (mut depth, mut start, mut in_string) = (0usize, 0, false);
    for (at, c) in list.char_indices() {
        match c {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth = depth.saturating_sub(1),
            ',' if !in_string && depth == 0 => {
                found.push(&list[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    found.push(&list[start..]);
    found
}

/// Whether `attribute` is a `#[cfg(…)]` whose predicate implies `test`: exactly
/// `test`, an `all(…)` with a `test` conjunct, or, as the one exception, exactly
/// `any(test, feature = "test-util")`, the feature a crate's own test targets
/// switch on for its test doubles.  Every other `any(…)` holds without `test`.
fn cfg_implies_test(attribute: &str) -> bool {
    let Some(predicate) = attribute
        .strip_prefix("#[cfg(")
        .and_then(|rest| rest.strip_suffix(")]"))
    else {
        return false;
    };
    let predicate: String = predicate.split_whitespace().collect();
    predicate == "test"
        || predicate == r#"any(test,feature="test-util")"#
        || predicate
            .strip_prefix("all(")
            .and_then(|rest| rest.strip_suffix(')'))
            .is_some_and(|conjuncts| top_level_arguments(conjuncts).contains(&"test"))
}

// ── The rules ─────────────────────────────────────────────────────────

/// `Ok` where `unmet` is empty, else the rule and what it lacks.
fn verdict(rule: &str, unmet: &[String]) -> Result<(), String> {
    if unmet.is_empty() {
        Ok(())
    } else {
        Err(format!("breaks rule {rule}: {}", unmet.join("; ")))
    }
}

/// Whether `name`, as an attribute lists it, is the lint: by its name or its
/// old one.
fn is_lint(name: &str) -> bool {
    name == LINT || name == OLD_LINT
}

/// Whether the attribute is an `allow` of the lint alone.
fn is_plain_allow(attribute: &Attribute) -> bool {
    attribute.name == "allow" && matches!(attribute.lints.as_slice(), [lint] if is_lint(lint))
}

/// Whether `source` is a whole unit of non-production code that carries a
/// crate-wide blanket: an integration-test crate root, an example, a build
/// script, or a guest init program's crate root.
fn is_blanket_root(source: &Source) -> bool {
    let within: Vec<&str> = source.within.split('/').collect();
    matches!(within.as_slice(), ["tests" | "examples", _] | ["build.rs"])
        || GUEST_INIT_ROOTS.contains(&source.rel.as_str())
}

/// Whether `code` before byte `upto` holds nothing but inner attributes: what
/// follows stands in the file's head, not in the middle of it.
fn heads_the_file(code: &str, upto: usize) -> bool {
    let mut at = 0;
    loop {
        let rest = code[at..upto].trim_start();
        if rest.is_empty() {
            return true;
        }
        let begin = upto - rest.len();
        match attribute_end(code, begin) {
            Some(end) if rest.starts_with("#![") && end <= upto => at = end,
            _ => return false,
        }
    }
}

/// Rule (b): an inner `allow` among the leading attributes of a blanket root.
fn rule_b(source: &Source, attribute: &Attribute) -> Result<(), String> {
    let mut unmet = Vec::new();
    if !is_plain_allow(attribute) {
        unmet.push(format!("it is not an `allow` of {LINT} alone"));
    }
    if attribute.reason.as_deref().is_none_or(str::is_empty) {
        unmet.push("it gives no reason".to_owned());
    }
    if !heads_the_file(&source.code, attribute.start) {
        unmet.push("it is not among the file's leading inner attributes".to_owned());
    }
    if !is_blanket_root(source) {
        unmet.push(
            "the file is not an integration-test root (`<crate>/tests/*.rs`), an example, \
             a `build.rs`, or the crate root of ral-guest"
                .to_owned(),
        );
    }
    verdict("(b), an inner allow", &unmet)
}

/// Rule (c): an outer `allow` with the reason "test code", on a `mod` item
/// whose `cfg` implies `test`.
fn rule_c(source: &Source, attribute: &Attribute) -> Result<(), String> {
    let mut unmet = Vec::new();
    if !is_plain_allow(attribute) || attribute.reason.as_deref() != Some("test code") {
        unmet.push(format!(
            "it is not `#[allow({LINT}, reason = \"test code\")]`"
        ));
    }
    let before = attributes_before(&source.code, attribute.start);
    let (after, head) = attributes_after(&source.code, attribute.end);
    if !is_mod_item(head) {
        unmet.push("it is not on a `mod` item".to_owned());
    } else if !before
        .iter()
        .chain(&after)
        .any(|attribute| cfg_implies_test(attribute))
    {
        unmet.push(
            "the module has no `#[cfg(…)]` whose predicate implies `test`: exactly `test`, an \
             `all(…)` with a `test` conjunct, or exactly `any(test, feature = \"test-util\")`"
                .to_owned(),
        );
    }
    verdict("(c), an outer allow", &unmet)
}

/// Rule (d): the `expect` on `tauri::generate_context!()` in `synod/src/main.rs`.
fn rule_d(source: &Source, attribute: &Attribute) -> Result<(), String> {
    let mut unmet = Vec::new();
    if attribute.name != "expect" {
        unmet.push("it is not an `expect`".to_owned());
    }
    let names = |lint: &str| attribute.lints.iter().any(|named| named == lint);
    if attribute.lints.len() != 2
        || !attribute.lints.iter().any(|named| is_lint(named))
        || !names(MACROS_LINT)
    {
        unmet.push(format!("it does not name exactly {LINT} and {MACROS_LINT}"));
    }
    if source.rel != TAURI_FILE {
        unmet.push(format!("the file is not {TAURI_FILE}"));
    }
    let (_, statement) = attributes_after(&source.code, attribute.end);
    if !statement.starts_with(TAURI_STATEMENT) {
        unmet.push("it does not precede `let context = tauri::generate_context!()`".to_owned());
    }
    verdict("(d), the third-party-codegen exception", &unmet)
}

/// Whether the occurrence of the lint's name at byte `at` of `source` is one of
/// the permitted four, or the rule it breaks.
fn judge(source: &Source, at: usize) -> Result<(), String> {
    let Some(attribute) = enclosing_attribute(&source.code, at) else {
        return Err(
            "names the lint outside an `allow` or `expect` attribute: a call is exempt \
                    only as a typed site (`lint/site.rs`) or under a blanket"
                .to_owned(),
        );
    };
    if attribute.inner {
        rule_b(source, &attribute)
    } else if attribute.name == "expect" {
        rule_d(source, &attribute)
    } else {
        rule_c(source, &attribute)
    }
}

/// The 1-based line of byte `at` in `code`.
fn line_of(code: &str, at: usize) -> usize {
    code[..at].bytes().filter(|&b| b == b'\n').count() + 1
}

#[test]
fn no_other_exemption_exists() {
    let mut failures = Vec::new();
    for source in workspace_sources() {
        for (at, _) in source.code.match_indices(OLD_LINT) {
            if let Err(why) = judge(&source, at) {
                failures.push(format!(
                    "{}:{}: {why}",
                    source.rel,
                    line_of(&source.code, at)
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} exemption(s) beyond typed sites and the blankets:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn no_group_silences_the_lint() {
    let mut failures = Vec::new();
    for source in workspace_sources() {
        for group in LINT_GROUPS {
            for (at, _) in source.code.match_indices(group) {
                let after = &source.code[at + group.len()..];
                if after.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                    // A longer lint's name, which this one begins.
                    continue;
                }
                let Some(attribute) = enclosing_attribute(&source.code, at) else {
                    continue;
                };
                if !matches!(attribute.name.as_str(), "deny" | "forbid") {
                    failures.push(format!(
                        "{}:{}: breaks the group rule: `#[{}(…)]` names {group}, which holds \
                         {LINT}; only `deny` and `forbid` may name the group, since any other \
                         level silences the lint unseen",
                        source.rel,
                        line_of(&source.code, at),
                        attribute.name,
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} attribute(s) silence the lint through its group:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Whether `manifest` has a `[lints]` table that holds `workspace = true`.
fn inherits_workspace_lints(manifest: &str) -> bool {
    let mut in_lints = false;
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            in_lints = line == "[lints]";
        } else if in_lints && line.replace(' ', "") == "workspace=true" {
            return true;
        }
    }
    false
}

#[test]
fn every_member_inherits_the_workspace_lints() {
    let root = workspace_root();
    let unbound: Vec<String> = members(&root)
        .into_iter()
        .map(|member| format!("{member}/Cargo.toml"))
        .filter(|manifest| {
            let text = std::fs::read_to_string(root.join(manifest))
                .unwrap_or_else(|e| panic!("read {manifest}: {e}"));
            !inherits_workspace_lints(&text)
        })
        .collect();
    assert!(
        unbound.is_empty(),
        "{} member(s) break the workspace-lints rule: no `[lints]` table with `workspace = true`, \
         so the workspace's bans do not bind them:\n{}",
        unbound.len(),
        unbound.join("\n")
    );
}

#[test]
fn the_string_containment_helpers_gain_no_caller() {
    let mut counted: BTreeMap<(String, &str), usize> = BTreeMap::new();

    for source in production_sources() {
        if source.rel.starts_with("core/src/path/") || source.rel == "core/src/path.rs" {
            continue;
        }
        let rendered = source.code.contains("Rendered");
        for name in STRING_CONTAINMENT_HELPERS
            .iter()
            .chain(RENDERED_NAME_HELPERS.iter().filter(|_| rendered))
        {
            let n = source.code.matches(name).count();
            if n > 0 {
                counted.insert((source.rel.clone(), name), n);
            }
        }
    }

    let pinned: BTreeMap<(String, &str), usize> = STRING_CONTAINMENT_CALLERS
        .iter()
        .map(|(f, name, n)| (((*f).to_owned(), *name), *n))
        .collect();
    assert_eq!(
        counted, pinned,
        "a string containment helper gained or lost a caller outside path/: these helpers serve \
         bwrap's envelope names (including surface spellings through links) and Seatbelt's \
         rendered firmlink ancestors, which are lexical names, not RealPath positions; no new \
         caller is admitted — update STRING_CONTAINMENT_CALLERS only when a caller goes."
    );
}
