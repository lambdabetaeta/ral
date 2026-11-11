//! Source-text helpers for the architecture rules in `tests/arch`.

#![allow(
    clippy::disallowed_methods,
    reason = "source scans, not production code"
)]

use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// An inline test module, which closes its file; attributes may sit between its
/// `#[cfg(test)]` and `mod`.
static INLINE_TESTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"#\[cfg\(test\)\]\s*(#\[[^\]]*\]\s*)*(pub(\([a-z]+\))?\s+)?mod\s+\w+\s*\{")
        .expect("the inline-test pattern is valid")
});

/// A path that starts at the crate root: `crate::x`, `crate::{`, or `super::`
/// repeated.
static ROOT_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(crate|super(?:\s*::\s*super)*)\s*::\s*(?:(\w+)|\{)")
        .expect("the root-reference pattern is valid")
});

/// The workspace root: the parent of this crate's directory.
///
/// # Panics
/// If the crate's directory has no parent.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("lint/ has a parent (the workspace root)")
        .to_path_buf()
}

/// Every `.rs` file under `dir`, sorted, but build output, dependencies and
/// hidden folders.
///
/// # Panics
/// If a directory cannot be read.
pub fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
            let (path, name) = (entry.path(), entry.file_name());
            if path.is_dir() {
                let skipped = name == "target" || name == "node_modules";
                if !skipped && !name.to_string_lossy().starts_with('.') {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Whether `line` contains `needle` as a whole word: `_` and ASCII
/// alphanumerics are word characters, so `tokio` matches in `tokio::select`
/// but `select` does not match inside `selector`.
pub fn contains_word(line: &str, needle: &str) -> bool {
    let bytes = line.as_bytes();
    let is_word = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let mut from = 0;
    while let Some(pos) = line[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_word(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Module path of `file` below `src`: `io/sink.rs` is `[io, sink]`.
///
/// # Panics
/// If `file` is not below `src`.
pub fn module_path(src: &Path, file: &Path) -> Vec<String> {
    let mut path: Vec<String> = file
        .strip_prefix(src)
        .expect("file is below src")
        .iter()
        .map(|p| p.to_string_lossy().trim_end_matches(".rs").to_owned())
        .collect();
    if path.last().is_some_and(|m| m == "mod") {
        path.pop();
    }
    path
}

/// `text` up to its inline test module, with `//` comments dropped.
pub fn production_code(text: &str) -> String {
    let live = INLINE_TESTS.find(text).map_or(text, |m| &text[..m.start()]);
    live.lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The top-level names of a `{ a, b::c, d::{e} }` group.
fn group_heads(rest: &str) -> Vec<String> {
    let (mut depth, mut top) = (0, String::new());
    for c in rest.chars() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => break,
            '}' => depth -= 1,
            c if depth == 0 => top.push(c),
            _ => {}
        }
    }
    top.split(',')
        .map(|item| {
            item.trim()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        })
        .collect()
}

/// The first segment below the crate root of every path in `code` that starts
/// at the root: `crate::x`, or `super::` exactly `depth` times (`depth` 0
/// admits `crate::` alone).
pub fn root_refs(code: &str, depth: usize) -> Vec<String> {
    ROOT_REFERENCE
        .captures_iter(code)
        .filter(|c| &c[1] == "crate" || c[1].matches("super").count() == depth)
        .flat_map(|c| match c.get(2) {
            Some(name) => vec![name.as_str().to_owned()],
            None => group_heads(&code[c.get_match().end()..]),
        })
        .collect()
}
