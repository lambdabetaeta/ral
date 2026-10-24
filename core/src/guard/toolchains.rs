//! The package managers a `dir` selector names, as one table.
//!
//! A [`Toolchain`] is a set of [`Location`]s.  A location's directories are
//! chosen from the host environment alone: the first usable [`Override`]
//! chooses and is all the location has, else every [`Base`] default is
//! chosen; only directories that exist are kept.  An override naming a
//! missing directory yields nothing, since the environment disowned the
//! defaults.  The rules are stated once, in `docs/ral-wiki/design/grant.md`
//! §Selectors; the table is the data.
//!
//! [`chosen`] is pure over an injected environment and home; [`expand_in`]
//! walks what it chose, refusing an override that names the world and keeping
//! a directory that is one, and [`expand`] is its live wrapper, which the
//! freeze and the fs floor read.

use super::freeze::{Home, PolicyError, Selector, refuse_the_world, the_world};
use crate::capability::GrantPath;
use crate::capability::Polar;
use crate::path::{
    FrozenPath, Kind, RealPath, Trace, is_absolute, join_slash, path_entries, shown,
};
use crate::path::{XdgKind, resolve_xdg_in};
use std::path::PathBuf;
use strum::{EnumString, IntoStaticStr, VariantArray};

/// A package manager, or `system`: the selector names `dir` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, VariantArray, EnumString, IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum Toolchain {
    System,
    Homebrew,
    Macports,
    Nix,
    Pkgsrc,
    Rustup,
    Cargo,
    Go,
    Nvm,
    Fnm,
    Volta,
    Pnpm,
    Npm,
    Bun,
    Deno,
    Pyenv,
    Uv,
    Pipx,
    Poetry,
    Pdm,
    Pipenv,
    Hatch,
    Virtualenvwrapper,
    Conda,
    Rbenv,
    Ghcup,
    Cabal,
    Stack,
    Opam,
    Elan,
    Juliaup,
    Sdkman,
    Jdk,
    Asdf,
    Mise,
    Dotnet,
    Coursier,
    Texlive,
    Scoop,
    Chocolatey,
    Winget,
}

/// The system-wide prefixes outside any home: world-readable trees of the
/// standing of `/usr`, so the read floor holds them.
pub const FLOOR: [Toolchain; 4] = [
    Toolchain::Homebrew,
    Toolchain::Macports,
    Toolchain::Nix,
    Toolchain::Pkgsrc,
];

/// One place a toolchain keeps what runs: tried overrides, then defaults.
pub struct Location {
    pub overrides: &'static [Override],
    pub defaults: &'static [Base],
}

/// A host environment variable and a `/`-separated subpath under its value.
#[derive(Clone, Copy)]
pub enum Override {
    Var(&'static str, &'static str),
    /// The first entry of a path-list variable.
    Head(&'static str, &'static str),
}

/// Where a default is anchored; the subpath is `/`-separated.
#[derive(Clone, Copy)]
pub enum Base {
    Abs(&'static str),
    Home(&'static str),
    Config(&'static str),
    Data(&'static str),
    Cache(&'static str),
}

macro_rules! loc {
    ([$($o:expr),*], [$($d:expr),*]) => {
        Location { overrides: &[$($o),*], defaults: &[$($d),*] }
    };
}

impl Toolchain {
    /// The table, one exhaustive `match`.
    #[allow(clippy::too_many_lines, reason = "the table is data")]
    pub fn locations(self) -> &'static [Location] {
        use Base::{Abs, Cache, Config, Data, Home};
        use Override::{Head, Var};
        match self {
            Self::System => &[
                loc!(
                    [],
                    [
                        Abs("/usr/bin"),
                        Abs("/bin"),
                        Abs("/usr/libexec"),
                        Abs("/usr/lib/git-core"),
                        Abs("/usr/lib/gcc"),
                        Abs("/Library/Developer/CommandLineTools"),
                        Abs("/Applications/Xcode.app/Contents/Developer")
                    ]
                ),
                loc!(
                    [Var("SystemRoot", "System32")],
                    [Abs(r"C:\Windows\System32")]
                ),
                loc!(
                    [Var("SystemRoot", "System32/WindowsPowerShell/v1.0")],
                    [Abs(r"C:\Windows\System32\WindowsPowerShell\v1.0")]
                ),
                loc!([Var("ProgramFiles", "Git/usr/bin")], []),
                loc!([Var("ProgramFiles(x86)", "Git/usr/bin")], []),
            ],
            Self::Homebrew => &[loc!(
                [Var("HOMEBREW_PREFIX", "")],
                [Abs("/opt/homebrew"), Abs("/home/linuxbrew/.linuxbrew")]
            )],
            Self::Macports => &[loc!([], [Abs("/opt/local")])],
            Self::Nix => &[loc!([], [Abs("/nix/store")])],
            Self::Pkgsrc => &[loc!([], [Abs("/usr/pkg"), Abs("/opt/pkg")])],
            Self::Rustup => &[loc!([Var("RUSTUP_HOME", "")], [Home(".rustup")])],
            Self::Cargo => &[loc!(
                [Var("CARGO_INSTALL_ROOT", "bin"), Var("CARGO_HOME", "bin")],
                [Home(".cargo/bin")]
            )],
            Self::Go => &[
                loc!(
                    [Var("GOROOT", "")],
                    [
                        Abs("/usr/local/go"),
                        Abs("/usr/lib/go"),
                        Abs("/usr/lib/golang")
                    ]
                ),
                loc!([Var("GOBIN", ""), Head("GOPATH", "bin")], [Home("go/bin")]),
            ],
            Self::Nvm => &[loc!([Var("NVM_DIR", "")], [Home(".nvm"), Config("nvm")])],
            Self::Fnm => &[loc!(
                [Var("FNM_DIR", "")],
                [
                    Data("fnm"),
                    Home(".fnm"),
                    Home("Library/Application Support/fnm")
                ]
            )],
            Self::Volta => &[loc!([Var("VOLTA_HOME", "")], [Home(".volta")])],
            Self::Pnpm => &[loc!(
                [Var("PNPM_HOME", "")],
                [Data("pnpm"), Home("Library/pnpm")]
            )],
            Self::Npm => &[loc!(
                [Var("npm_config_prefix", "lib/node_modules")],
                [
                    Abs("/usr/local/lib/node_modules"),
                    Abs("/usr/lib/node_modules"),
                    Home(".local/lib/node_modules")
                ]
            )],
            Self::Bun => &[loc!([Var("BUN_INSTALL", "")], [Home(".bun")])],
            Self::Deno => &[
                loc!([Var("DENO_INSTALL", "")], [Home(".deno")]),
                loc!([Var("DENO_INSTALL_ROOT", "bin")], [Home(".deno/bin")]),
            ],
            Self::Pyenv => &[loc!([Var("PYENV_ROOT", "")], [Home(".pyenv")])],
            Self::Uv => &[
                loc!([Var("UV_TOOL_DIR", "")], [Data("uv/tools")]),
                loc!([Var("UV_PYTHON_INSTALL_DIR", "")], [Data("uv/python")]),
            ],
            Self::Pipx => &[loc!(
                [Var("PIPX_HOME", "")],
                [
                    Data("pipx"),
                    Home(".local/pipx"),
                    Home("Library/Application Support/pipx")
                ]
            )],
            Self::Poetry => &[
                loc!(
                    [Var("POETRY_HOME", "")],
                    [
                        Data("pypoetry"),
                        Home("Library/Application Support/pypoetry")
                    ]
                ),
                loc!(
                    [
                        Var("POETRY_VIRTUALENVS_PATH", ""),
                        Var("POETRY_CACHE_DIR", "virtualenvs")
                    ],
                    [
                        Cache("pypoetry/virtualenvs"),
                        Home("Library/Caches/pypoetry/virtualenvs")
                    ]
                ),
            ],
            Self::Pdm => &[loc!(
                [Var("PDM_HOME", "")],
                [Data("pdm"), Home("Library/Application Support/pdm")]
            )],
            Self::Pipenv => &[loc!([Var("WORKON_HOME", "")], [Data("virtualenvs")])],
            Self::Hatch => &[loc!(
                [Var("HATCH_DATA_DIR", "env")],
                [
                    Data("hatch/env"),
                    Home("Library/Application Support/hatch/env")
                ]
            )],
            Self::Virtualenvwrapper => &[loc!([Var("WORKON_HOME", "")], [Home(".virtualenvs")])],
            Self::Conda => &[
                loc!(
                    [],
                    [
                        Home("miniforge3"),
                        Home("miniconda3"),
                        Home("anaconda3"),
                        Home("mambaforge"),
                        Abs("/opt/anaconda3"),
                        Abs("/opt/conda")
                    ]
                ),
                loc!([], [Home(".conda/envs")]),
                loc!(
                    [Var("MAMBA_ROOT_PREFIX", "")],
                    [Data("mamba"), Home("micromamba")]
                ),
            ],
            Self::Rbenv => &[loc!([Var("RBENV_ROOT", "")], [Home(".rbenv")])],
            Self::Ghcup => &[loc!(
                [Var("GHCUP_INSTALL_BASE_PREFIX", ".ghcup")],
                [Home(".ghcup"), Data("ghcup")]
            )],
            Self::Cabal => &[loc!([Var("CABAL_DIR", "bin")], [Home(".cabal/bin")])],
            Self::Stack => &[loc!(
                [Var("STACK_ROOT", "programs")],
                [Home(".stack/programs"), Data("stack/programs")]
            )],
            Self::Opam => &[loc!([Var("OPAMROOT", "")], [Home(".opam")])],
            Self::Elan => &[loc!([Var("ELAN_HOME", "")], [Home(".elan")])],
            Self::Juliaup => &[
                loc!([Var("JULIAUP_BIN_DIR", "")], [Home(".juliaup/bin")]),
                loc!(
                    [Var("JULIAUP_DEPOT_PATH", "juliaup")],
                    [Home(".julia/juliaup")]
                ),
            ],
            Self::Sdkman => &[loc!([Var("SDKMAN_DIR", "")], [Home(".sdkman")])],
            Self::Jdk => &[
                loc!(
                    [],
                    [
                        Abs("/Library/Java/JavaVirtualMachines"),
                        Home("Library/Java/JavaVirtualMachines"),
                        Abs("/usr/lib/jvm"),
                        Home(".jdks")
                    ]
                ),
                loc!([Var("JAVA_HOME", "")], []),
            ],
            Self::Asdf => &[loc!([Var("ASDF_DATA_DIR", "")], [Home(".asdf")])],
            Self::Mise => &[loc!([Var("MISE_DATA_DIR", "")], [Data("mise")])],
            Self::Dotnet => &[
                loc!(
                    [Var("DOTNET_ROOT", "")],
                    [
                        Home(".dotnet"),
                        Abs("/usr/local/share/dotnet"),
                        Abs("/usr/share/dotnet"),
                        Abs("/usr/lib/dotnet"),
                        Abs("/usr/lib64/dotnet")
                    ]
                ),
                loc!(
                    [Var("DOTNET_CLI_HOME", ".dotnet/tools")],
                    [Home(".dotnet/tools")]
                ),
            ],
            Self::Coursier => &[loc!(
                [
                    Var("COURSIER_BIN_DIR", ""),
                    Var("COURSIER_INSTALL_DIR", ""),
                    Var("COURSIER_DATA_DIR", "bin")
                ],
                [
                    Data("coursier/bin"),
                    Home("Library/Application Support/Coursier/bin")
                ]
            )],
            Self::Texlive => &[loc!([], [Abs("/usr/local/texlive")])],
            Self::Scoop => &[
                loc!([Var("SCOOP", "shims")], [Home("scoop/shims")]),
                loc!([Var("SCOOP", "apps")], [Home("scoop/apps")]),
            ],
            Self::Chocolatey => &[
                loc!(
                    [Var("ChocolateyInstall", "bin")],
                    [Abs(r"C:\ProgramData\chocolatey\bin")]
                ),
                loc!(
                    [Var("ChocolateyInstall", "lib")],
                    [Abs(r"C:\ProgramData\chocolatey\lib")]
                ),
            ],
            Self::Winget => &[loc!([Var("LOCALAPPDATA", "Microsoft/WinGet/Packages")], [])],
        }
    }
}

impl Override {
    fn var(self) -> &'static str {
        let (Self::Var(var, _) | Self::Head(var, _)) = self;
        var
    }

    fn sub(self) -> &'static str {
        let (Self::Var(_, sub) | Self::Head(_, sub)) = self;
        sub
    }

    /// The directory the variable names, if usable: non-empty and absolute
    /// under the host's rules.  An empty or relative one is skipped, as
    /// `path` skips a relative `$PATH` entry.
    fn value(self, env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
        let raw = env(self.var())?;
        let value = match self {
            Self::Var(..) => PathBuf::from(raw),
            Self::Head(..) => path_entries(raw.as_ref()).next()?,
        };
        is_absolute(&value).then_some(value)
    }
}

impl Base {
    /// The directory, or `None` where its anchor is unbound or it is not
    /// absolute under the host's rules: a foreign-rooted default belongs to
    /// the other platform.
    fn dir(self, env: &impl Fn(&str) -> Option<String>, home: Option<&str>) -> Option<PathBuf> {
        let xdg = |kind| resolve_xdg_in(env, kind, home);
        let (root, sub) = match self {
            Self::Abs(path) => (PathBuf::from(path), ""),
            Self::Home(sub) => (PathBuf::from(home?), sub),
            Self::Config(sub) => (xdg(XdgKind::Config)?, sub),
            Self::Data(sub) => (xdg(XdgKind::Data)?, sub),
            Self::Cache(sub) => (xdg(XdgKind::Cache)?, sub),
        };
        Some(join_slash(root, sub)).filter(|dir| is_absolute(dir))
    }
}

/// The variable that chose a directory, and its value as given.
type Via = (&'static str, PathBuf);

/// A directory walked, beside the override that chose it.
type Found<W> = (GrantPath<W>, Option<Via>);

/// A directory a location chooses, before anything is walked: and, where the
/// environment chose it, the variable that did and its value as given.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Chosen {
    pub(super) dir: PathBuf,
    via: Option<Via>,
}

impl Location {
    /// The directories chosen: the first usable override alone, else every
    /// default.
    fn chosen(&self, env: &impl Fn(&str) -> Option<String>, home: Option<&str>) -> Vec<Chosen> {
        let Some((over, value)) = (self.overrides.iter()).find_map(|&o| Some((o, o.value(env)?)))
        else {
            return (self.defaults.iter())
                .filter_map(|b| b.dir(env, home))
                .map(|dir| Chosen { dir, via: None })
                .collect();
        };
        let dir = join_slash(&value, over.sub());
        vec![Chosen {
            dir,
            via: Some((over.var(), value)),
        }]
    }
}

/// The directories `toolchain` chooses over an injected environment and
/// home, native as the OS takes them, before any is walked.
pub(super) fn chosen(
    toolchain: Toolchain,
    env: impl Fn(&str) -> Option<String>,
    home: Option<&str>,
) -> Vec<Chosen> {
    (toolchain.locations().iter())
        .flat_map(|l| l.chosen(&env, home))
        .collect()
}

/// Each directory `toolchain` chooses, walked as `W` walks, beside the
/// override that chose it; one absent or no directory is dropped.
///
/// # Errors
/// A directory whose walk fails, or that no grant can hold.
fn walked<W: Polar>(
    toolchain: Toolchain,
    env: impl Fn(&str) -> Option<String>,
    home: Option<&str>,
) -> Result<Vec<Found<W>>, PolicyError> {
    let mut kept = Vec::new();
    for Chosen { dir, via } in chosen(toolchain, env, home) {
        let written = FrozenPath::freeze(&dir).map_err(PolicyError::new)?;
        let walked = GrantPath::<W>::walked(written)?;
        if walked.real.followed().kind() == Some(Kind::Dir) {
            kept.push((walked, via));
        }
    }
    Ok(kept)
}

/// The directories `toolchain` expands to over an injected environment and
/// home, each walked as polarity `W` walks: an override that names the
/// filesystem root, the home or an ancestor of it is refused, and a directory
/// that is absent or no directory is dropped.  A toolchain that is absent
/// expands to nothing.
///
/// # Errors
/// An override that names the world; a directory whose walk fails, or that no
/// grant can hold.
pub(crate) fn expand_in<W: Polar>(
    toolchain: Toolchain,
    env: impl Fn(&str) -> Option<String>,
    home: Option<&Home>,
) -> Result<Vec<GrantPath<W::Grant>>, PolicyError> {
    walked::<W>(toolchain, env, home.map(|h| h.spelled.as_str()))?
        .into_iter()
        .map(|(dir, via)| {
            if let Some((var, value)) = via {
                let subject = format!("{var} is {}", shown(&value));
                refuse_the_world(
                    Selector::Toolchain(toolchain),
                    &subject,
                    var,
                    dir.real.followed(),
                    home,
                )?;
            }
            W::grant(dir)
        })
        .collect()
}

/// [`expand_in`] over the host environment and filesystem, as the spellings
/// that survive.
///
/// # Errors
/// As [`expand_in`]; a home that cannot be walked.
pub fn expand(toolchain: Toolchain, home: Option<&str>) -> Result<Vec<PathBuf>, PolicyError> {
    let home = home.map(Home::of).transpose()?;
    let kept = expand_in::<Trace>(toolchain, crate::host::var, home.as_ref())?;
    Ok(kept
        .iter()
        .map(|dir| PathBuf::from(dir.written.as_str()))
        .collect())
}

/// The [`FLOOR`] rows' directories, walked, less any in the home, the user's
/// to grant, or [`the_world`]: dropped, never refused, since an override is
/// refused only where a grant names it.  A walk that fails refuses the floor,
/// named: a host that cannot resolve its own floor cannot run anything.
///
/// # Errors
/// A directory whose walk fails, or that no grant can hold.
pub(crate) fn floor_in(
    env: impl Fn(&str) -> Option<String>,
    home: Option<&Home>,
) -> Result<Vec<GrantPath<Trace>>, PolicyError> {
    let held = home.and_then(|h| h.real.as_ref());
    let floor =
        |dir: &RealPath| the_world(dir, held).is_none() && held.is_none_or(|h| !h.contains(dir));
    let mut kept = Vec::new();
    for toolchain in FLOOR {
        for (dir, _) in walked::<Trace>(toolchain, &env, home.map(|h| h.spelled.as_str()))? {
            if floor(dir.real.real()) {
                kept.push(dir);
            }
        }
    }
    Ok(kept)
}

/// [`floor_in`] over the host environment and filesystem.
///
/// # Errors
/// A home that cannot be walked; as [`floor_in`].
pub(crate) fn floor(home: Option<&str>) -> Result<Vec<GrantPath<Trace>>, String> {
    let home = home.map(Home::of).transpose().map_err(|e| e.message)?;
    floor_in(crate::host::var, home.as_ref()).map_err(|e| e.message)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::guard::freeze::Anchor;
    #[cfg(unix)]
    use crate::path::Subject;
    use std::path::Path;

    /// A directory as the fixtures spell it.
    fn spelled(dir: &Path) -> &str {
        crate::path::utf8(dir).expect("a UTF-8 fixture")
    }

    /// [`chosen`] as the strings the fixtures spell.
    fn chosen_in(
        toolchain: Toolchain,
        env: impl Fn(&str) -> Option<String>,
        home: Option<&str>,
    ) -> Vec<String> {
        (chosen(toolchain, env, home).iter())
            .map(|c| spelled(&c.dir).to_owned())
            .collect()
    }

    /// [`expand_in`] as the spellings it keeps, the home walked where it is
    /// spelled.
    fn expanded(
        toolchain: Toolchain,
        env: impl Fn(&str) -> Option<String>,
        home: Option<&str>,
    ) -> Result<Vec<String>, PolicyError> {
        let home = home.map(Home::of).transpose()?;
        let dirs = expand_in::<Trace>(toolchain, env, home.as_ref())?;
        Ok(dirs.iter().map(|d| d.written.as_str().to_owned()).collect())
    }

    /// [`floor_in`] as the spellings it keeps.
    #[cfg(unix)]
    fn floor_of(env: impl Fn(&str) -> Option<String>, home: &str) -> Vec<String> {
        let home = Home::of(home).unwrap();
        let dirs = floor_in(env, Some(&home)).unwrap();
        dirs.iter().map(|d| d.written.as_str().to_owned()).collect()
    }

    /// A tempdir with `dirs` made beneath it, and its root as the fixtures
    /// spell it.
    fn tree(dirs: &[&str]) -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        for dir in dirs {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        let root = spelled(tmp.path()).to_owned();
        (tmp, root)
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find_map(|&(var, v)| (var == k).then(|| v.to_owned()))
        }
    }

    #[test]
    fn names_are_closed_and_clear_of_the_place_tags() {
        for &t in Toolchain::VARIANTS {
            let name = <&str>::from(t);
            assert_eq!(name.parse(), Ok(t));
            assert_eq!(Anchor::of_tag(name), None, "{name} is a place tag");
            assert_ne!(name, "path");
        }
    }

    /// Every row against an empty environment and `/h`: the defaults, by
    /// anchor.
    #[cfg(unix)]
    #[test]
    fn defaults() {
        let cases: &[(Toolchain, &[&str])] = &[
            (
                Toolchain::System,
                &[
                    "/usr/bin",
                    "/bin",
                    "/usr/libexec",
                    "/usr/lib/git-core",
                    "/usr/lib/gcc",
                    "/Library/Developer/CommandLineTools",
                    "/Applications/Xcode.app/Contents/Developer",
                ],
            ),
            (
                Toolchain::Homebrew,
                &["/opt/homebrew", "/home/linuxbrew/.linuxbrew"],
            ),
            (Toolchain::Macports, &["/opt/local"]),
            (Toolchain::Nix, &["/nix/store"]),
            (Toolchain::Pkgsrc, &["/usr/pkg", "/opt/pkg"]),
            (Toolchain::Rustup, &["/h/.rustup"]),
            (Toolchain::Cargo, &["/h/.cargo/bin"]),
            (
                Toolchain::Go,
                &[
                    "/usr/local/go",
                    "/usr/lib/go",
                    "/usr/lib/golang",
                    "/h/go/bin",
                ],
            ),
            (Toolchain::Nvm, &["/h/.nvm", "/h/.config/nvm"]),
            (
                Toolchain::Fnm,
                &[
                    "/h/.local/share/fnm",
                    "/h/.fnm",
                    "/h/Library/Application Support/fnm",
                ],
            ),
            (
                Toolchain::Poetry,
                &[
                    "/h/.local/share/pypoetry",
                    "/h/Library/Application Support/pypoetry",
                    "/h/.cache/pypoetry/virtualenvs",
                    "/h/Library/Caches/pypoetry/virtualenvs",
                ],
            ),
            (
                Toolchain::Uv,
                &["/h/.local/share/uv/tools", "/h/.local/share/uv/python"],
            ),
            (Toolchain::Deno, &["/h/.deno", "/h/.deno/bin"]),
            (
                Toolchain::Conda,
                &[
                    "/h/miniforge3",
                    "/h/miniconda3",
                    "/h/anaconda3",
                    "/h/mambaforge",
                    "/opt/anaconda3",
                    "/opt/conda",
                    "/h/.conda/envs",
                    "/h/.local/share/mamba",
                    "/h/micromamba",
                ],
            ),
            (Toolchain::Ghcup, &["/h/.ghcup", "/h/.local/share/ghcup"]),
            (
                Toolchain::Stack,
                &["/h/.stack/programs", "/h/.local/share/stack/programs"],
            ),
            (
                Toolchain::Jdk,
                &[
                    "/Library/Java/JavaVirtualMachines",
                    "/h/Library/Java/JavaVirtualMachines",
                    "/usr/lib/jvm",
                    "/h/.jdks",
                ],
            ),
            (
                Toolchain::Dotnet,
                &[
                    "/h/.dotnet",
                    "/usr/local/share/dotnet",
                    "/usr/share/dotnet",
                    "/usr/lib/dotnet",
                    "/usr/lib64/dotnet",
                    "/h/.dotnet/tools",
                ],
            ),
            (Toolchain::Scoop, &["/h/scoop/shims", "/h/scoop/apps"]),
            (Toolchain::Texlive, &["/usr/local/texlive"]),
            (Toolchain::Winget, &[]),
        ];
        for &(t, want) in cases {
            assert_eq!(chosen_in(t, env(&[]), Some("/h")), want, "{t:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_override_replaces_only_its_own_location() {
        let got = chosen_in(Toolchain::Go, env(&[("GOROOT", "/opt/go")]), Some("/h"));
        assert_eq!(got, ["/opt/go", "/h/go/bin"]);
        let got = chosen_in(Toolchain::Deno, env(&[("DENO_INSTALL", "/d")]), Some("/h"));
        assert_eq!(got, ["/d", "/h/.deno/bin"]);
    }

    #[cfg(unix)]
    #[test]
    fn overrides_are_tried_in_order() {
        let go =
            |pairs: &[(&str, &str)]| chosen_in(Toolchain::Go, env(pairs), Some("/h"))[3..].to_vec();
        assert_eq!(go(&[("GOBIN", "/b"), ("GOPATH", "/p")]), ["/b"]);
        assert_eq!(go(&[("GOPATH", "/p")]), ["/p/bin"]);
        let cargo = |pairs: &[(&str, &str)]| chosen_in(Toolchain::Cargo, env(pairs), Some("/h"));
        assert_eq!(
            cargo(&[("CARGO_INSTALL_ROOT", "/i"), ("CARGO_HOME", "/c")]),
            ["/i/bin"]
        );
        assert_eq!(cargo(&[("CARGO_HOME", "/c")]), ["/c/bin"]);
    }

    /// The environment said where the manager is: no fallback to defaults,
    /// though the defaults are there.
    #[cfg(unix)]
    #[test]
    fn a_chosen_override_naming_a_missing_directory_yields_nothing() {
        let (_tmp, root) = tree(&["h/.cargo/bin"]);
        let home = format!("{root}/h");
        let missing = format!("{root}/c");
        let cargo = [("CARGO_HOME", missing.as_str())];
        assert_eq!(
            chosen_in(Toolchain::Cargo, env(&cargo), Some(&home)),
            [format!("{missing}/bin")]
        );
        let got = expanded(Toolchain::Cargo, env(&cargo), Some(&home));
        assert_eq!(got.unwrap(), Vec::<String>::new());
        let prefix = format!("{root}/prefix");
        let brew = [("HOMEBREW_PREFIX", prefix.as_str())];
        assert_eq!(
            chosen_in(Toolchain::Homebrew, env(&brew), Some(&home)),
            std::slice::from_ref(&prefix)
        );
        let got = expanded(Toolchain::Homebrew, env(&brew), Some(&home));
        assert_eq!(got.unwrap(), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_first_gopath_entry_counts() {
        let got = chosen_in(Toolchain::Go, env(&[("GOPATH", "/p:/q")]), Some("/h"));
        assert_eq!(got[3..], ["/p/bin"]);
    }

    #[cfg(unix)]
    #[test]
    fn empty_and_relative_values_are_ignored() {
        for bad in ["", "rel/c", "./c"] {
            let got = chosen_in(
                Toolchain::Cargo,
                env(&[("CARGO_HOME", bad), ("CARGO_INSTALL_ROOT", bad)]),
                Some("/h"),
            );
            assert_eq!(got, ["/h/.cargo/bin"], "{bad:?}");
        }
        let got = chosen_in(
            Toolchain::Homebrew,
            env(&[("HOMEBREW_PREFIX", "brew")]),
            None,
        );
        assert_eq!(got, ["/opt/homebrew", "/home/linuxbrew/.linuxbrew"]);
    }

    #[cfg(unix)]
    #[test]
    fn data_defaults_follow_xdg_data_home() {
        let got = chosen_in(Toolchain::Fnm, env(&[("XDG_DATA_HOME", "/x")]), Some("/h"));
        assert_eq!(
            got,
            ["/x/fnm", "/h/.fnm", "/h/Library/Application Support/fnm"]
        );
        let got = chosen_in(Toolchain::Fnm, env(&[("XDG_DATA_HOME", "rel")]), Some("/h"));
        assert_eq!(
            got,
            [
                "/h/.local/share/fnm",
                "/h/.fnm",
                "/h/Library/Application Support/fnm"
            ]
        );
    }

    /// A selector reports what is installed: with no home, home defaults are
    /// absent, and an absolute `XDG_*_HOME` alone still answers.
    #[cfg(unix)]
    #[test]
    fn an_unbound_home_leaves_home_defaults_absent() {
        let chosen = |t, pairs: &[(&str, &str)]| chosen_in(t, env(pairs), None);
        assert_eq!(chosen(Toolchain::Rustup, &[]), Vec::<String>::new());
        assert_eq!(chosen(Toolchain::Fnm, &[]), Vec::<String>::new());
        assert_eq!(
            chosen(Toolchain::Fnm, &[("XDG_DATA_HOME", "/x")]),
            ["/x/fnm"]
        );
        assert_eq!(chosen(Toolchain::Nix, &[]), ["/nix/store"]);
        let got = chosen_in(Toolchain::Rustup, env(&[]), Some("rel"));
        assert_eq!(got, Vec::<String>::new());
    }

    #[test]
    fn foreign_rooted_defaults_are_skipped() {
        let (foreign, native): (&[Toolchain], &[Toolchain]) = if cfg!(windows) {
            (
                &[Toolchain::Homebrew, Toolchain::Macports],
                &[Toolchain::Chocolatey],
            )
        } else {
            (&[Toolchain::Chocolatey], &[Toolchain::Macports])
        };
        for &t in foreign {
            assert_eq!(chosen_in(t, env(&[]), None), Vec::<String>::new(), "{t:?}");
        }
        for &t in native {
            assert!(!chosen_in(t, env(&[]), None).is_empty(), "{t:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_override_naming_the_world_is_refused() {
        let (_tmp, root) = tree(&["h", "home/a"]);
        for (home, value, what) in [
            (format!("{root}/h"), "/".to_owned(), "the filesystem root"),
            (
                format!("{root}/h"),
                format!("{root}/h"),
                "the home directory itself",
            ),
            (
                format!("{root}/h"),
                format!("{root}/h/../h"),
                "the home directory itself",
            ),
            (
                format!("{root}/home/a"),
                format!("{root}/home"),
                "an ancestor of the home directory",
            ),
        ] {
            let err = expanded(
                Toolchain::Rustup,
                env(&[("RUSTUP_HOME", &value)]),
                Some(&home),
            )
            .unwrap_err();
            assert_eq!(
                err.message,
                format!(
                    "RUSTUP_HOME is {value}, {what}, so `` `dir `rustup `` would run and read \
                     all of it. Is RUSTUP_HOME set correctly?"
                ),
            );
        }
    }

    /// The check is on the directory, not the variable.
    #[cfg(unix)]
    #[test]
    fn a_variable_naming_the_home_is_fine_where_its_subpath_goes_below() {
        let (_tmp, root) = tree(&["h/.ghcup", "h/bin"]);
        let home = format!("{root}/h");
        let expand = |t, var, v: &str| expanded(t, env(&[(var, v)]), Some(&home)).unwrap();
        assert_eq!(
            expand(Toolchain::Ghcup, "GHCUP_INSTALL_BASE_PREFIX", &home),
            [format!("{home}/.ghcup")]
        );
        assert_eq!(
            expand(Toolchain::Go, "GOPATH", &home).last(),
            Some(&format!("{home}/bin"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_floor_holds_the_system_prefixes_and_not_a_prefix_in_the_home() {
        let (_tmp, root) = tree(&["h/linuxbrew", "elsewhere"]);
        let home = format!("{root}/h");
        let floor = |pairs: &[(&str, &str)]| floor_of(env(pairs), &home);
        let prefixes = [
            "/opt/homebrew",
            "/opt/local",
            "/nix/store",
            "/usr/pkg",
            "/opt/pkg",
        ];
        let held = floor(&[]);
        assert!(
            held.iter().all(|p| prefixes.contains(&p.as_str())),
            "{held:?}"
        );
        // What the other rows hold on this host, the homebrew row disowned.
        let gone = format!("{root}/gone");
        let rest = floor(&[("HOMEBREW_PREFIX", &gone)]);
        let elsewhere = format!("{root}/elsewhere");
        assert_eq!(
            floor(&[("HOMEBREW_PREFIX", &elsewhere)]),
            [vec![elsewhere.clone()], rest.clone()].concat()
        );
        let in_home = format!("{home}/linuxbrew");
        assert_eq!(floor(&[("HOMEBREW_PREFIX", &in_home)]), rest);
        // A row that fails is dropped, the others stay.
        assert_eq!(floor(&[("HOMEBREW_PREFIX", "/")]), rest);
    }

    #[cfg(unix)]
    #[test]
    fn a_floor_directory_whose_walk_fails_refuses_the_floor() {
        let (_tmp, root) = tree(&[]);
        let looped = format!("{root}/loop");
        std::os::unix::fs::symlink("loop", &looped).unwrap();
        let err = floor_in(env(&[("HOMEBREW_PREFIX", &looped)]), None).unwrap_err();
        assert!(err.message.contains(&looped), "{}", err.message);
    }

    #[test]
    fn an_override_naming_the_world_or_the_home_is_no_floor_and_no_error() {
        let (tmp, root) = tree(&["h"]);
        let home_dir = spelled(&tmp.path().join("h")).to_owned();
        let home = Home::of(&home_dir).unwrap();
        let held = home.real.clone().unwrap();
        let kept = |prefix: &str, home: Option<&Home>| {
            floor_in(env(&[("HOMEBREW_PREFIX", prefix)]), home).unwrap()
        };
        let host_root = if cfg!(windows) { r"C:\" } else { "/" };
        for home in [None, Some(&home)] {
            let floor = kept(host_root, home);
            assert!(floor.iter().all(|d| !d.real.real().is_root()), "{floor:?}");
        }
        for prefix in [home_dir.as_str(), root.as_str()] {
            let floor = kept(prefix, Some(&home));
            assert!(
                floor
                    .iter()
                    .all(|d| !held.contains(d.real.real()) && !d.real.real().contains(&held)),
                "{prefix}: {floor:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn system_is_kept_by_existence_and_carries_no_homebrew() {
        let (_tmp, root) = tree(&["Git/usr", "real"]);
        let git = format!("{root}/Git/usr/bin");
        let pairs = [("ProgramFiles", root.as_str())];
        let system = || expanded(Toolchain::System, env(&pairs), None).unwrap();
        let table = chosen_in(Toolchain::System, env(&pairs), None);
        assert!(table.contains(&git), "{table:?}");
        assert!(table.iter().all(|d| !d.contains("brew")), "{table:?}");
        assert!(!system().contains(&git), "an absent directory is dropped");
        std::fs::write(&git, "x").unwrap();
        assert!(!system().contains(&git), "a file is dropped");
        std::fs::remove_file(&git).unwrap();
        std::os::unix::fs::symlink(format!("{root}/real"), &git).unwrap();
        assert!(system().contains(&git), "a link to a directory is kept");
        std::fs::remove_file(&git).unwrap();
        std::fs::create_dir(&git).unwrap();
        assert!(system().contains(&git), "a directory is kept");
        let present: Vec<_> = table
            .into_iter()
            .filter(|d| Path::new(d).is_dir())
            .collect();
        assert_eq!(system(), present);
    }

    /// Rustup resolves `cargo` under `~/.rustup/toolchains/`, a path shape
    /// easy to miss when hand-enumerating exec roots.
    #[cfg(unix)]
    #[test]
    fn rustup_covers_a_toolchain_cargo() {
        let (_tmp, root) = tree(&["h/.rustup"]);
        let home = Home::of(&format!("{root}/h")).unwrap();
        let roots = expand_in::<Trace>(Toolchain::Rustup, env(&[]), Some(&home)).unwrap();
        let cargo = crate::test_helper::trace(format!(
            "{root}/h/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo"
        ));
        assert!(
            roots
                .iter()
                .any(|r| r.real.contains(Subject::Access(&cargo))),
            "{roots:?}"
        );
    }

    /// With nothing of any manager in the home, what a toolchain keeps is
    /// what the host has outside it.
    #[test]
    fn an_absent_manager_expands_to_nothing() {
        let (tmp, _) = tree(&["h"]);
        let home = spelled(&tmp.path().join("h")).to_owned();
        for &t in Toolchain::VARIANTS {
            for dir in expanded(t, env(&[]), Some(&home)).unwrap() {
                assert!(Path::new(&dir).is_dir(), "{t:?}: {dir}");
                assert!(!dir.starts_with(&home), "{t:?}: {dir}");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn system_on_windows() {
        let system = |pairs: &[(&str, &str)]| chosen_in(Toolchain::System, env(pairs), None);
        let base = [
            r"C:\Windows\System32",
            r"C:\Windows\System32\WindowsPowerShell\v1.0",
        ];
        assert_eq!(system(&[("SystemRoot", r"C:\Windows")]), base);
        assert_eq!(system(&[]), base);
        let git = r"C:\Program Files\Git\usr\bin";
        let pf = [("ProgramFiles", r"C:\Program Files")];
        assert_eq!(system(&pf), [base[0], base[1], git]);
        let x86 = [("ProgramFiles(x86)", r"C:\Program Files (x86)")];
        let git86 = r"C:\Program Files (x86)\Git\usr\bin";
        assert_eq!(system(&x86), [base[0], base[1], git86]);
    }
}
