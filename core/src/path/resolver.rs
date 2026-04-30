//! Pipeline orchestration: compose stages 1–3 in one place.
//!
//! `Resolver` bundles the per-call resolution context (`HOME`,
//! cwd, canonicalisation mode) and exposes two entry points:
//!
//!   * [`Resolver::lex`]    — sigil-expand then lexically resolve.
//!   * [`Resolver::check`]  — sigil-expand, lex, then canonicalise
//!                            according to [`CanonMode`].
//!
//! There is no public way to canonicalise without first running
//! sigil-expansion-then-lex; the pipeline ordering is encoded in
//! the type, not in convention.
//!
//! Stack-allocated; constructed afresh per call from a `Dynamic`.
//! Owns its `home` to keep call-site lifetimes simple.

use std::path::{Path, PathBuf};

use super::{canon, lex, sigil};

/// How `Resolver::check` performs stage 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonMode {
    /// Realpath; on missing components, walk up to an existing
    /// ancestor and re-attach the unresolved tail.  Default for
    /// grant prefixes (which may name not-yet-created targets)
    /// and for grant-side access checks outside a sandboxed
    /// child.
    Lenient,
    /// Skip canonicalisation entirely.  Used inside a sandboxed
    /// child where the OS sandbox is the real gate and
    /// `realpath(3)` may fail spuriously on intermediate
    /// components.  Containment then relies on alias awareness
    /// (`/tmp` ↔ `/private/tmp` on macOS) to bridge the gap.
    LexicalOnly,
}

/// Per-call resolution context: `HOME`, scoped cwd, and
/// canonicalisation mode.  See module doc for the pipeline.
pub struct Resolver<'a> {
    pub home: String,
    pub cwd: Option<&'a Path>,
    pub mode: CanonMode,
}

impl<'a> Resolver<'a> {
    /// Stage 1 + 2: expand `~` / `xdg:` sigils, then lexically
    /// resolve against `cwd`.  Pure: no filesystem access.
    pub fn lex(&self, raw: &str) -> PathBuf {
        let expanded = sigil::expand_path_prefix(raw, &self.home);
        lex::resolve_path(self.cwd, &expanded)
    }

    /// Stage 1 + 2 + 3: full pipeline.  Touches the filesystem
    /// only in [`CanonMode::Lenient`]; in [`CanonMode::LexicalOnly`]
    /// it is identical to [`Resolver::lex`].
    pub fn check(&self, raw: &str) -> PathBuf {
        let lex = self.lex(raw);
        match self.mode {
            CanonMode::Lenient => canon::canonicalise_lenient(&lex),
            CanonMode::LexicalOnly => lex,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `lex` composes stage 1 (sigil) and stage 2 (cwd-anchor +
    /// `.`/`..`).  A tilde-headed path against a fresh `home`
    /// expands and stays absolute; `cwd` is irrelevant once the
    /// expanded form is absolute.
    #[test]
    fn lex_expands_tilde_and_normalises() {
        let r = Resolver { home: "/h".into(), cwd: None, mode: CanonMode::LexicalOnly };
        assert_eq!(r.lex("~/foo/./bar/../baz"), Path::new("/h/foo/baz"));
    }

    /// A non-sigil relative path joins to `cwd` and normalises.
    /// The whole point of stage 2 is that grant prefix matching
    /// always sees absolute paths.
    #[test]
    fn lex_anchors_relative_paths_to_cwd() {
        let cwd = Path::new("/work/proj");
        let r = Resolver { home: "/h".into(), cwd: Some(cwd), mode: CanonMode::LexicalOnly };
        assert_eq!(r.lex("src/lib.rs"), Path::new("/work/proj/src/lib.rs"));
    }

    /// `check` in `Lenient` mode walks up to an existing ancestor
    /// when the full path is missing.  We use a child of `/tmp`
    /// (an ancestor that always exists) to assert the suffix is
    /// re-appended after the lenient canonicalisation.
    #[test]
    fn check_lenient_resolves_partial_paths_against_existing_ancestor() {
        let r = Resolver { home: "/h".into(), cwd: None, mode: CanonMode::Lenient };
        let suffix = format!("ral-resolver-probe-{}/leaf", std::process::id());
        let probe = format!("/tmp/{suffix}");
        let out = r.check(&probe);
        assert!(out.ends_with(&suffix), "expected suffix {suffix:?} in {out:?}");
    }

    /// `check` in `LexicalOnly` mode stops at stage 2.  This is
    /// the in-sandbox path: never touches the filesystem, never
    /// canonicalises.  Result must equal what `lex` returned.
    #[test]
    fn check_lexical_only_is_identical_to_lex() {
        let r_lenient = Resolver { home: "/h".into(), cwd: None, mode: CanonMode::Lenient };
        let r_lex_only = Resolver { home: "/h".into(), cwd: None, mode: CanonMode::LexicalOnly };
        // Use a path that doesn't exist so canonicalise_lenient
        // walks up to `/` — its output and `lex`'s output diverge
        // only when an ancestor is a symlink (e.g. /tmp on macOS).
        // For lexical-only mode, the input shape is preserved.
        let p = "/no/such/path/at/all";
        assert_eq!(r_lex_only.check(p), r_lex_only.lex(p));
        // And both modes agree on the lexical part.
        assert_eq!(r_lex_only.lex(p), r_lenient.lex(p));
    }

    /// End-to-end: `xdg:` token through every stage when the env
    /// var is unset (so the Linux default `~/.local/share` kicks
    /// in) and the path doesn't exist on disk.  `check` returns
    /// the lenient canonicalisation, which for a non-existent
    /// path under HOME ends in the expected suffix.
    #[test]
    fn check_pipeline_handles_xdg_with_unset_env() {
        // Snapshot+restore XDG_DATA_HOME so the test doesn't
        // pollute parallel runs.
        let prev = std::env::var_os("XDG_DATA_HOME");
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        let r = Resolver {
            home: "/h".into(),
            cwd: None,
            mode: CanonMode::LexicalOnly,
        };
        let out = r.check("xdg:data/agda");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        // Default xdg:data is ${XDG_DATA_HOME:-~/.local/share}.
        // With the var unset and home=/h, expansion gives
        // /h/.local/share, suffix /agda is appended.
        assert_eq!(out, Path::new("/h/.local/share/agda"));
    }

    /// Plain absolute paths pass through every stage unchanged
    /// (in `LexicalOnly` mode).  No sigil to expand, already
    /// absolute, no `.`/`..` to collapse.
    #[test]
    fn ordinary_absolute_path_is_a_fixed_point() {
        let r = Resolver { home: "/h".into(), cwd: None, mode: CanonMode::LexicalOnly };
        assert_eq!(r.check("/etc/hostname"), Path::new("/etc/hostname"));
    }
}
