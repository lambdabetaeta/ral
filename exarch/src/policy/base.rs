//! The six built-in capability bases, embedded from the `.ral` scripts in
//! `exarch/data/` and loaded through
//! [`load_capabilities_from_str`](ral_core::load::profile::load_capabilities_from_str),
//! the same surface the ral CLI's `--capabilities` flag consumes.
//!
//! Loosest to tightest: `dangerous` (the lattice top), `reasonable`,
//! `edit-only`, `read-only`, `minimal`, `confined`.  Every profile but
//! `dangerous` names `cwd` and `tempdir` places, which the loader freezes at
//! session start — so the per-invocation working directory is baked into the
//! policy and exarch never injects it dynamically.

use ral_core::authority::grant::freeze::FreezeCtx;
use ral_core::authority::policy::GrantLayer;
use ral_core::io::terminal::TerminalState;
use ral_core::source::SourceOrigin;

const MINIMAL_RAL: &str = include_str!("../../data/minimal.exarch.ral");
const REASONABLE_RAL: &str = include_str!("../../data/reasonable.exarch.ral");
const READ_ONLY_RAL: &str = include_str!("../../data/read-only.exarch.ral");
const EDIT_ONLY_RAL: &str = include_str!("../../data/edit-only.exarch.ral");
const CONFINED_RAL: &str = include_str!("../../data/confined.exarch.ral");
const DANGEROUS_RAL: &str = include_str!("../../data/dangerous.exarch.ral");
// Gated with its only consumers, the Unix-shaped extension tests below.
#[cfg(all(test, unix))]
const GIT_EXTENSION_RAL: &str = include_str!("../../examples/git.exarch.ral");

/// Resolve `name` to a [`GrantLayer`], every spelling walked against `ctx`, so
/// `super::for_invocation` composes on already-resolved bundles.  A bare exec
/// name this host finds nowhere, and bundles not at all, makes no allowance.
pub(super) fn resolve_base(name: &str, ctx: &FreezeCtx) -> Result<GrantLayer, String> {
    let text = match name {
        "minimal" => MINIMAL_RAL,
        "reasonable" => REASONABLE_RAL,
        "read-only" => READ_ONLY_RAL,
        "edit-only" => EDIT_ONLY_RAL,
        "confined" => CONFINED_RAL,
        "dangerous" => DANGEROUS_RAL,
        other => {
            return Err(format!(
                "unknown base '{other}'; \
                 expected one of: dangerous, reasonable, edit-only, read-only, minimal, confined"
            ));
        }
    };
    let mut shell = ral_core::HostSurface::default()
        .shell(TerminalState::default(), ctx.cwd().clone())
        .map_err(|e| format!("built-in base '{name}' cannot load: {e}"))?;
    let origin = SourceOrigin::named(format!("<built-in:{name}>"));
    ral_core::load::profile::load_capabilities_from_str(
        &ral_core::types::Mooring::adrift(),
        &mut shell,
        text,
        &origin,
        ctx,
    )
    .map_err(|e| match e {
        ral_core::types::Break::Error(err) => {
            format!("built-in base '{name}' failed to parse: {}", err.message)
        }
        other @ ral_core::types::Break::Escape(_) => {
            format!("built-in base '{name}' failed: {other:?}")
        }
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use ral_core::authority::grant::freeze::FreezeCtx;
    #[cfg(unix)]
    use ral_core::authority::grant::freeze::Place;
    #[cfg(unix)]
    use ral_core::authority::policy::{Capabilities, Fragment, Net};
    use ral_core::authority::policy::{ExecSummary, FsSummary, GrantLayer};
    #[cfg(unix)]
    use ral_core::path::XdgKind;
    use std::path::PathBuf;

    #[cfg(unix)]
    use std::path::Path;

    /// The fs spelling a grant shows for the frozen `path`.
    fn spelled(path: &str) -> String {
        ral_core::path::shown(ral_core::path::FrozenPath::from_surface(path).as_str())
    }

    /// The exec key a grant writes for the directory `path`.
    fn dir_key(path: &str) -> String {
        format!(
            "`dir `absolute {}",
            ral_core::syntax::quote_word(&spelled(path))
        )
    }

    /// The exec key a grant writes for the bare `name`.
    fn name_key(name: &str) -> String {
        format!("`name {}", ral_core::syntax::quote_word(name))
    }

    /// The fs rules `layer` shows.
    fn fs_of(layer: &GrantLayer) -> FsSummary {
        layer
            .summary()
            .and_then(|s| s.fs)
            .expect("the layer declares fs")
    }

    /// The exec rules `layer` shows.
    fn exec_of(layer: &GrantLayer) -> ExecSummary {
        layer
            .summary()
            .and_then(|s| s.exec)
            .expect("the layer declares exec")
    }

    /// The scratch spelling every test anchor carries: no test makes anything
    /// in it.
    fn scratch() -> PathBuf {
        std::env::temp_dir().join("a-session-scratch")
    }

    /// A freeze anchor whose overlay binds `HOME` alone, at `cwd`, over
    /// [`scratch`].
    fn ctx_at(home: &str, cwd: impl AsRef<std::path::Path>) -> FreezeCtx {
        let cwd = ral_core::path::CwdPath::new(cwd.as_ref().to_path_buf())
            .expect("a fixture cwd is absolute");
        FreezeCtx::at(
            std::iter::once(("HOME", home)).collect::<ral_core::types::EnvVars>(),
            cwd,
        )
        .with_scratch(Some(scratch().to_string_lossy()))
    }

    /// Parse *and* freeze a bake-in: the loader resolves places, so a failure
    /// here is malformed source, a refused payload, or an xdg escape.
    fn load(name: &str, text: &str, ctx: &FreezeCtx) -> GrantLayer {
        let mut shell = ral_core::test_helper::core_shell();
        ral_core::load::profile::load_capabilities_from_str(
            &ral_core::types::Mooring::adrift(),
            &mut shell,
            text,
            &SourceOrigin::named(format!("<test-base:{name}>")),
            ctx,
        )
        .unwrap_or_else(|e| panic!("base '{name}' failed to load: {e:?}"))
    }

    /// [`load`] for an extension file.
    #[cfg(unix)]
    fn fragment(name: &str, text: &str, ctx: &FreezeCtx) -> Fragment {
        let mut shell = ral_core::test_helper::core_shell();
        ral_core::load::profile::load_fragment_from_str(
            &ral_core::types::Mooring::adrift(),
            &mut shell,
            text,
            &SourceOrigin::named(format!("<test-fragment:{name}>")),
            ctx,
        )
        .unwrap_or_else(|e| panic!("extension '{name}' failed to load: {e:?}"))
    }

    /// Expect a grant through the path that authored it.  `expand_path_prefix`
    /// is the runtime resolver's infallible twin and normalises differently,
    /// so an equality against it agrees with a frozen grant only by
    /// coincidence — and disagrees exactly where the freeze would have failed.
    #[cfg(unix)]
    fn frozen(place: &Place, ctx: &FreezeCtx) -> ral_core::path::FrozenPath {
        ctx.place(place)
            .ok()
            .flatten()
            .unwrap_or_else(|| panic!("{place:?} should freeze"))
    }

    #[cfg(unix)]
    fn xdg(kind: XdgKind, sub: &str) -> Place {
        Place::Xdg(kind, sub.into())
    }

    /// The real path of the file at the host-held `path`, as the in-process
    /// exec guard judges it.
    #[cfg(unix)]
    fn object(path: &Path) -> String {
        ral_core::test_helper::trace(path).real().to_string()
    }

    /// The real path of the `name` the host `PATH` finds, if any.
    #[cfg(unix)]
    fn host_real(name: &str) -> Option<String> {
        let path = std::env::var("PATH").ok()?;
        let hit = ral_core::path::locate(name, Some(&path), None)?;
        Some(object(&hit))
    }

    /// Whether `layer` admits running the file whose real path is `real`.
    #[cfg(unix)]
    fn admits(layer: GrantLayer, real: &str) -> bool {
        let mut shell = ral_core::test_helper::core_shell();
        shell.with_layer(layer, |sh| ral_core::test_access::admits_file(sh, real))
    }

    /// Every bake-in names `home`/`xdg-*` places, so a host with no `HOME` cannot
    /// load one at all — said here rather than by each test failing obscurely.
    fn host_home() -> String {
        ral_core::host::home()
            .expect("these profiles name `home`/`xdg-*` places, so the test host needs HOME")
    }

    /// A base profile is walked once, by the loader that makes its layer:
    /// `resolve_base` takes no step the loader did not.
    #[cfg(unix)]
    #[test]
    fn a_base_is_walked_once() {
        let ctx = ctx_at(&host_home(), "/");
        for (name, text) in [("reasonable", REASONABLE_RAL), ("confined", CONFINED_RAL)] {
            let steps = ral_core::test_access::walk_steps;
            let before = steps();
            let loaded = load(name, text, &ctx);
            let loader = steps() - before;
            assert!(loader > 0, "{name}: the loader walks its places");
            let before = steps();
            let resolved = resolve_base(name, &ctx).expect("a bake-in resolves");
            assert_eq!(steps() - before, loader, "{name}: walked again");
            assert_eq!(resolved, loaded, "{name}");
        }
    }

    /// Every bake-in must load against the real `HOME`, so a broken profile
    /// fails at `cargo test` rather than at a user's first invocation.
    #[cfg(unix)]
    #[test]
    fn bakeins_parse() {
        let home = host_home();
        let ctx = ctx_at(&home, "/");
        for (name, text) in [
            ("minimal", MINIMAL_RAL),
            ("reasonable", REASONABLE_RAL),
            ("read-only", READ_ONLY_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("confined", CONFINED_RAL),
            ("dangerous", DANGEROUS_RAL),
        ] {
            load(name, text, &ctx);
        }
    }

    /// Every bake-in carries Unix-only literals (`/tmp`, `/opt/homebrew/`, …).
    /// On Windows they are foreign-rooted and must drop as dead grants rather
    /// than trip the freeze pass's absoluteness check, or the whole profile
    /// would refuse to load and `dangerous` would be the only usable base.
    /// Gated because that branch fires only under a real `cfg!(windows)`.
    #[cfg(windows)]
    #[test]
    fn bakeins_parse_on_windows() {
        let home = host_home();
        let ctx = ctx_at(&home, r"C:\work");
        for (name, text) in [
            ("minimal", MINIMAL_RAL),
            ("reasonable", REASONABLE_RAL),
            ("read-only", READ_ONLY_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("confined", CONFINED_RAL),
            ("dangerous", DANGEROUS_RAL),
        ] {
            load(name, text, &ctx);
        }
    }

    /// The drop reaches fs literals and exec-dir overrides alike: a grant that
    /// can never match a real access must not show up as authority.
    #[cfg(windows)]
    #[test]
    fn minimal_drops_foreign_rooted_grants_on_windows() {
        let home = host_home();
        let ctx = ctx_at(&home, r"C:\work");
        let layer = load("minimal", MINIMAL_RAL, &ctx);
        assert!(
            !fs_of(&layer).read.iter().any(|p| p == "/tmp"),
            "the Unix-only '/tmp' literal must not survive freeze on Windows"
        );
        assert!(
            !exec_of(&layer)
                .allows
                .iter()
                .any(|k| k == "`dir `absolute /opt/homebrew"),
            "the foreign-rooted '/opt/homebrew/' override must not survive freeze on Windows"
        );
    }

    /// The three load-bearing differences from `reasonable`, pinned so an edit
    /// cannot quietly widen the build jail.  `confined` names no `home`/`xdg-*`
    /// place, so a synthetic home suffices — and nothing may resolve under it.
    #[cfg(unix)]
    #[test]
    fn confined_is_offline_subpath_only_no_home_reads() {
        let ctx = ctx_at("/h", "/work/proj");
        let layer = load("confined", CONFINED_RAL, &ctx);
        assert_eq!(
            layer.summary().and_then(|s| s.net),
            Some(Net::Offline),
            "confined must have net off"
        );
        let bundled: Vec<String> = ral_core::uutils::bundled_tools()
            .into_iter()
            .map(name_key)
            .collect();
        let allows = exec_of(&layer).allows;
        let host_named: Vec<&str> = (allows.iter())
            .filter(|key| key.starts_with("`absolute ") || key.starts_with("`name "))
            .filter(|key| !bundled.contains(key))
            .map(String::as_str)
            .collect();
        assert!(
            host_named.is_empty(),
            "confined names host binaries {host_named:?}; the build jail admits those by \
             directory prefix, and only the bundled tools (which have no path) are listed"
        );
        let fs = fs_of(&layer);
        for prefix in fs.read.iter().chain(&fs.write) {
            assert!(
                !prefix.starts_with("/h"),
                "confined fs prefix '{prefix}' reaches into home: build jail must not"
            );
        }
    }

    /// The one difference from `reasonable`: the working tree is readable but
    /// not writable.  Guards against `cwd` creeping back into `write`.
    #[cfg(unix)]
    #[test]
    fn read_only_does_not_write_cwd() {
        let home = host_home();
        let ctx = ctx_at(&home, "/work/proj");
        let fs = fs_of(&load("read-only", READ_ONLY_RAL, &ctx));
        assert!(
            !fs.write.contains(&spelled("/work/proj")),
            "read-only must not write the working tree"
        );
        assert!(
            fs.read.contains(&spelled("/work/proj")),
            "read-only must read the working tree"
        );
    }

    #[test]
    fn dangerous_is_root() {
        let ctx = ctx_at("/h", "/");
        assert_eq!(load("dangerous", DANGEROUS_RAL, &ctx), GrantLayer::root());
    }

    /// `xdg-bin` freezes to `${XDG_BIN_HOME:-~/.local/bin}` and must land in
    /// the grant's `dirs`, where directory keys live.
    #[cfg(unix)]
    #[test]
    fn reasonable_carries_xdg_bin_subpath_in_exec() {
        let home = host_home();
        let ctx = ctx_at(&home, "/");
        let exec = exec_of(&load("reasonable", REASONABLE_RAL, &ctx));
        let xdg_bin = frozen(&xdg(XdgKind::Bin, "."), &ctx);
        assert!(
            exec.allows.contains(&dir_key(xdg_bin.as_str())),
            "reasonable should list the resolved xdg-bin ({}) in exec dirs",
            xdg_bin.as_str()
        );
    }

    /// The per-invocation working tree, temp dir and scratch reach both halves
    /// of the policy: directory keys in `exec`, plain sigils in `fs`.
    #[cfg(unix)]
    #[test]
    fn minimal_and_reasonable_carry_cwd_tempdir_and_scratch_sigils() {
        let home = host_home();
        let cwd = Path::new("/work/proj");
        let ctx = ctx_at(&home, cwd);
        // Freeze folds away the trailing separator macOS `$TMPDIR` carries, so
        // compare in the same normal form the frozen keys hold.
        let cwd_resolved = spelled(&cwd.to_string_lossy());
        let tempdir_resolved = spelled(&std::env::temp_dir().to_string_lossy());
        let scratch_resolved = spelled(&scratch().to_string_lossy());
        let tokens = [&cwd_resolved, &tempdir_resolved, &scratch_resolved];
        for (name, text) in [("minimal", MINIMAL_RAL), ("reasonable", REASONABLE_RAL)] {
            let layer = load(name, text, &ctx);
            let exec = exec_of(&layer);
            for token in tokens {
                assert!(
                    exec.allows.contains(&dir_key(token)),
                    "{name} exec missing resolved {token}"
                );
            }
            let fs = fs_of(&layer);
            for token in tokens {
                assert!(fs.read.contains(token), "{name} fs.read missing {token}");
                assert!(fs.write.contains(token), "{name} fs.write missing {token}");
            }
        }
    }

    /// A directory outside every root `minimal` admits by place: `/tmp` and
    /// the scratch dir are admitted wholesale, so a fixture in either proves
    /// nothing.
    #[cfg(unix)]
    fn outside_tmp() -> tempfile::TempDir {
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("a fixture dir beside the crate")
    }

    /// End-to-end: once freeze rewrites `cwd` into the working directory, a
    /// path-style exec beneath it is admitted.
    #[cfg(unix)]
    #[test]
    fn freeze_admits_relative_exec_under_cwd_sigil() {
        let work = outside_tmp();
        let tree = ral_core::test_helper::Tree::lay(work.path(), ["/configure"]);
        let ctx = ctx_at(&host_home(), tree.root());
        let layer = load("minimal", MINIMAL_RAL, &ctx);

        assert!(
            admits(layer, &tree.at("/configure")),
            "./configure under `cwd` must be admitted"
        );
    }

    /// `system`'s Homebrew root admits cmake, though cmake is no per-name
    /// entry.  That root exists only where Homebrew does, so on a brew-less
    /// host this passes vacuously rather than failing falsely; CI's macOS
    /// runners ship Homebrew, so it still bites.
    #[cfg(unix)]
    #[test]
    fn reasonable_admits_cmake_under_homebrew_when_present() {
        if !ral_core::path::exists("/opt/homebrew") {
            return;
        }
        let home = host_home();
        let ctx = ctx_at(&home, "/");
        let layer = load("reasonable", REASONABLE_RAL, &ctx);
        assert!(
            exec_of(&layer).allows.contains(&dir_key("/opt/homebrew")),
            "reasonable should list /opt/homebrew in exec dirs when it exists on this host"
        );
        assert!(
            admits(layer, "/opt/homebrew/bin/cmake"),
            "cmake under /opt/homebrew/bin must be admitted"
        );
    }

    /// The selector names `text`'s `exec` allow list targets as `` `dir `NAME ``,
    /// read from the evaluated profile before it is decoded and frozen.
    fn dir_selectors(text: &str) -> std::collections::BTreeSet<String> {
        use ral_core::types::Value;
        let mut shell = ral_core::test_helper::core_shell();
        let profile = ral_core::load::evaluate_source(
            &ral_core::types::Mooring::adrift(),
            &mut shell,
            text,
            &SourceOrigin::named("<test-base:selectors>"),
            None,
        )
        .unwrap_or_else(|e| panic!("profile failed to evaluate: {e:?}"));
        let field = |v: &Value, k: &str| match v {
            Value::Map(m) => m.get(k).map(std::borrow::Cow::into_owned),
            _ => None,
        };
        let allow = field(&profile, "exec")
            .and_then(|exec| field(&exec, "allow"))
            .expect("the profile has an exec allow list");
        let Value::List(targets) = allow else {
            panic!("exec allow is a list");
        };
        targets
            .iter()
            .filter_map(|t| match &*t {
                Value::Variant { label, payload } if &**label == "dir" => {
                    payload.as_deref().cloned()
                }
                _ => None,
            })
            .filter_map(|inner| match inner {
                Value::Variant {
                    label,
                    payload: None,
                } => Some(label.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Every selector, `path` included, is listed under `dir`: read off the
    /// profile's own value, since what a selector expands to depends on the
    /// host, and a manager absent here would pass vacuously.
    #[test]
    fn reasonable_lists_every_selector() {
        use ral_core::authority::grant::freeze::Selector;
        let want = Selector::names().map(String::from).collect();
        assert_eq!(dir_selectors(REASONABLE_RAL), want);
    }

    /// A session writes no cache the user's own tools read.
    #[cfg(unix)]
    #[test]
    fn reasonable_and_minimal_write_no_user_cache() {
        let ctx = ctx_at(&host_home(), "/");
        let cache = frozen(&xdg(XdgKind::Cache, "."), &ctx);
        for (name, text) in [("minimal", MINIMAL_RAL), ("reasonable", REASONABLE_RAL)] {
            let fs = fs_of(&load(name, text, &ctx));
            assert!(
                !fs.write.contains(&spelled(cache.as_str())),
                "{name} must not write {}",
                cache.as_str()
            );
        }
    }

    /// Both admit `git` and read `~/.gitconfig`, so a commit works without
    /// `--extend`.  Push and signed commits still don't: their credential
    /// and signing helpers need `~/.ssh` and `~/.gnupg`, which the fs grant
    /// leaves unreadable.
    #[cfg(unix)]
    #[test]
    fn reasonable_and_read_only_admit_git() {
        let home = host_home();
        let ctx = ctx_at(&home, "/");
        let gitconfig = frozen(&Place::Home(".gitconfig".into()), &ctx);
        for (name, text) in [("reasonable", REASONABLE_RAL), ("read-only", READ_ONLY_RAL)] {
            let layer = load(name, text, &ctx);
            if let Some(git) = host_real("git") {
                assert!(
                    exec_of(&layer).allows.contains(&name_key("git")),
                    "{name} should name git"
                );
                assert!(admits(layer.clone(), &git), "{name} should admit git");
            }
            assert!(
                fs_of(&layer).read.contains(&spelled(gitconfig.as_str())),
                "{name} should add resolved ~/.gitconfig ({}) to read prefixes",
                gitconfig.as_str()
            );
        }
    }

    /// `minimal` lists no Homebrew tree and `system` carries none, so brew
    /// tools stay opt-in, and the git extension is the opt-in — its bare
    /// `git` is whichever git the host `PATH` finds.  The cwd must not be `/`,
    /// or the ``dir `cwd .`` allow would admit everything.
    #[cfg(unix)]
    #[test]
    fn minimal_admits_system_git_not_homebrew() {
        let home = host_home();
        let elsewhere = outside_tmp();
        let tree = ral_core::test_helper::Tree::lay(elsewhere.path(), ["/opt/homebrew/bin/git"]);
        let ctx = ctx_at(&home, "/work");
        assert!(
            !admits(
                load("minimal", MINIMAL_RAL, &ctx),
                &tree.at("/opt/homebrew/bin/git")
            ),
            "minimal does not admit a Homebrew git: brew trees are opt-in"
        );
        // `system` names `/usr/bin` itself, so the system git is judged where it is.
        if ral_core::path::exists("/usr/bin/git") {
            assert!(
                admits(load("minimal", MINIMAL_RAL, &ctx), "/usr/bin/git"),
                "minimal admits the system git via the /usr/bin/ subpath"
            );
        } else {
            eprintln!("skipped: /usr/bin/git is absent on this host");
        }
        let extended =
            load("minimal", MINIMAL_RAL, &ctx).extend(fragment("git-ext", GIT_EXTENSION_RAL, &ctx));
        if let Some(git) = host_real("git") {
            assert!(
                exec_of(&extended).allows.contains(&name_key("git")),
                "the git extension names git"
            );
            assert!(
                admits(extended, &git),
                "the git extension's `name git` carries the host's git, wherever it lives"
            );
        }
    }

    /// The extension extends `minimal` — gitconfig readable, `git` admitted —
    /// while `reasonable`'s credential denies survive: extending an fs layer
    /// unions deny sets, so a veto sticks without the overlay re-stating it, which is
    /// what lets an overlay stay purely additive.  Both sides freeze against
    /// the same home, so their `xdg-config` places coincide.
    #[cfg(unix)]
    #[test]
    fn git_extension_extends_into_git_capable_profile() {
        let home = host_home();
        let ctx = ctx_at(&home, "/");
        let gitconfig = frozen(&Place::Home(".gitconfig".into()), &ctx);
        let xdg_config_git = frozen(&xdg(XdgKind::Config, "git"), &ctx);

        let extended_minimal = load("minimal", MINIMAL_RAL, &ctx).extend(fragment(
            "git-extension",
            GIT_EXTENSION_RAL,
            &ctx,
        ));
        if host_real("git").is_some() {
            assert!(exec_of(&extended_minimal).allows.contains(&name_key("git")));
        }
        let fs = fs_of(&extended_minimal);
        assert!(
            fs.read.contains(&spelled(gitconfig.as_str())),
            "extending minimal should add ~/.gitconfig"
        );
        assert!(
            fs.read.contains(&spelled(xdg_config_git.as_str())),
            "extending minimal should add xdg-config git"
        );

        let extended_reasonable = load("reasonable", REASONABLE_RAL, &ctx).extend(fragment(
            "git-extension",
            GIT_EXTENSION_RAL,
            &ctx,
        ));
        let fs = fs_of(&extended_reasonable);
        for denied in ["gh", "op", "gcloud"] {
            let resolved = frozen(&xdg(XdgKind::Config, denied), &ctx);
            assert!(
                fs.deny.contains(&spelled(resolved.as_str())),
                "extending reasonable should preserve xdg-config {denied} deny ({})",
                resolved.as_str()
            );
        }
    }

    /// A positive extension reaches a target the base never listed, and an
    /// area it does not name stays as the base left it.
    #[cfg(unix)]
    #[test]
    fn a_positive_extension_of_an_unlisted_target_is_admitted() {
        let home = host_home();
        let ctx = ctx_at(&home, "/work");
        let tool = object(&std::path::PathBuf::from(&home).join("unlisted-tool"));
        let minimal = load("minimal", MINIMAL_RAL, &ctx);
        assert!(!admits(minimal.clone(), &tool), "{tool} is not minimal's");
        let extended = minimal.extend(fragment(
            "home-dir",
            "return [exec: [allow: [`dir `home .]]]",
            &ctx,
        ));
        assert!(
            admits(extended, &tool),
            "the extension names {tool}'s folder"
        );
    }

    /// An exclusion wins over every allowance: an exact file or a deeper
    /// directory in an extension does not reopen `minimal`'s `bash`.
    #[cfg(unix)]
    #[test]
    fn neither_an_exact_file_nor_a_deeper_directory_reopens_an_exclusion() {
        let ctx = ctx_at(&host_home(), "/work");
        for source in [
            "return [exec: [allow: [`absolute /usr/bin/bash]]]",
            "return [exec: [allow: [`dir `absolute /usr/bin]]]",
        ] {
            let extended =
                load("minimal", MINIMAL_RAL, &ctx).extend(fragment("reopen", source, &ctx));
            assert!(
                !admits(extended, "/usr/bin/bash"),
                "`{source}` must not reopen the `name bash` exclusion"
            );
        }
    }

    /// Verb lists join by union: a base cannot withhold a verb against an
    /// extension.
    #[cfg(unix)]
    #[test]
    fn verbs_join_by_union() {
        use ral_core::authority::policy::{Flag, ShellVerb};
        use ral_core::test_helper::{layer, stack};
        let ctx = ctx_at(&host_home(), "/work");
        let base = layer(Capabilities {
            shell: Some([ShellVerb::Chdir].into()),
            ..Capabilities::default()
        });
        let detach = Flag::Shell(ShellVerb::Detach);
        assert!(!stack([base.clone()]).permits(detach));
        let extended = base.extend(fragment("verbs", "return [shell: [`detach]]", &ctx));
        assert!(stack([extended]).permits(detach));
    }

    /// A base that omits exec but restricts fs, extended with a run allowance
    /// beside `every`: the ceiling keeps the allowance and reads what it
    /// names.  The program sits in a scratch dir, outside the system reads.
    #[cfg(unix)]
    #[test]
    fn a_run_allowance_beside_every_is_readable_under_an_fs_only_base() {
        let ctx = ctx_at(&host_home(), "/work");
        let base = ral_core::test_helper::layer(Capabilities {
            fs: Some(ral_core::authority::policy::FsPolicy {
                read_prefixes: vec![frozen(&Place::Absolute("/work".into()), &ctx)],
                ..Default::default()
            }),
            ..Capabilities::default()
        });
        let dir = tempfile::tempdir().expect("scratch dir");
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").expect("write the tool");
        let tool = object(&tool);
        assert!(!admits(base.clone(), &tool), "{tool} is outside the reads");
        let extended = base.extend(fragment(
            "tool",
            &format!("return [exec: [allow: [`absolute {tool}]]]"),
            &ctx,
        ));
        assert!(admits(extended, &tool), "the allowance reads what it names");
    }

    /// `system` must expand to an allowance for every live tool root —
    /// merely finding the key would pass on a deny too, which is the whole
    /// point.  Ungated, since the expansion is the host's own and the
    /// assertion holds either way.
    #[test]
    fn every_exec_base_admits_live_system_tool_roots() {
        let home = host_home();
        // Absolute on every platform, unlike the `/work` literal the
        // `cfg(unix)`-gated tests use — Windows reads that as rootless.
        let cwd = std::env::temp_dir();
        let ctx = ctx_at(&home, cwd);
        let roots = ral_core::authority::grant::toolchains::expand(
            ral_core::authority::grant::toolchains::Toolchain::System,
            Some(&home),
        )
        .expect("the system roots expand");
        for (name, text) in [
            ("reasonable", REASONABLE_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("read-only", READ_ONLY_RAL),
            ("minimal", MINIMAL_RAL),
            ("confined", CONFINED_RAL),
        ] {
            let exec = exec_of(&load(name, text, &ctx));
            for root in &roots {
                let key = dir_key(ral_core::path::utf8(root).unwrap());
                assert!(
                    exec.allows.contains(&key),
                    "{name} should allow the live system tool root {key}"
                );
            }
        }
    }

    /// A bundled tool (`ral_core::uutils`) is spawned by bare name and never
    /// resolved on disk, so a directory prefix can never admit one: a profile
    /// that omits one leaves an agent unable to run it. Every base a *spawn*
    /// may name must therefore settle each bundled tool by literal — `allow`
    /// or `deny`, but never silence, which denies it with no decision recorded.
    /// The list is [`super::SPAWN_BASES`], the bases a spawn may name, which
    /// no base; `dangerous` is absent because it declares no exec map to
    /// restrict.
    #[test]
    fn every_grantable_base_settles_the_bundled_tools() {
        let home = host_home();
        let cwd = std::env::temp_dir();
        let ctx = ctx_at(&home, cwd);
        for (name, text) in [
            ("reasonable", REASONABLE_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("read-only", READ_ONLY_RAL),
            ("confined", CONFINED_RAL),
        ] {
            let exec = exec_of(&load(name, text, &ctx));
            let unsettled: Vec<&str> = ral_core::uutils::bundled_tools()
                .into_iter()
                .filter(|tool| {
                    let key = name_key(tool);
                    !exec.allows.contains(&key) && !exec.denies.contains(&key)
                })
                .collect();
            assert!(
                unsettled.is_empty(),
                "{name} leaves these bundled tools unnamed: a bare name matches no prefix, \
                 so silence makes them unrunnable: {unsettled:?}"
            );
        }
    }
}
