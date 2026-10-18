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
//! [`expand_in`] is pure over an injected environment, home and `exists`;
//! [`expand`] is its live wrapper, which the freeze and the fs floor read.

use super::freeze::{PolicyError, Selector, refuse_the_world};
use crate::path::{Allow, FrozenPath, is_absolute};
use crate::path::{XdgKind, resolve_xdg_in};
use std::ffi::OsStr;
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

    /// The variable's value if usable: non-empty and absolute under the
    /// host's rules.  An empty or relative one is skipped, as `path` skips a
    /// relative `$PATH` entry.
    fn value(self, env: &impl Fn(&str) -> Option<String>) -> Option<String> {
        let raw = env(self.var())?;
        let value = match self {
            Self::Var(..) => raw,
            Self::Head(..) => std::env::split_paths(&raw).next()?.to_str()?.to_owned(),
        };
        (!value.is_empty() && is_absolute(&value)).then_some(value)
    }
}

impl Base {
    /// The directory, or `None` where its anchor is unbound or it is not
    /// absolute under the host's rules: a foreign-rooted default belongs to
    /// the other platform.
    fn dir(self, env: &impl Fn(&str) -> Option<String>, home: Option<&str>) -> Option<String> {
        let xdg = |kind| resolve_xdg_in(env, kind, home).map(|p| p.to_string_lossy().into_owned());
        let (root, sub) = match self {
            Self::Abs(path) => (path.to_owned(), ""),
            Self::Home(sub) => (home?.to_owned(), sub),
            Self::Config(sub) => (xdg(XdgKind::Config)?, sub),
            Self::Data(sub) => (xdg(XdgKind::Data)?, sub),
            Self::Cache(sub) => (xdg(XdgKind::Cache)?, sub),
        };
        Some(join(&root, sub)).filter(|dir| is_absolute(dir))
    }
}

/// `sub` joined to `root` per component, as `resolve_xdg` joins its suffixes.
fn join(root: impl AsRef<OsStr>, sub: &str) -> String {
    (sub.split('/').filter(|c| !c.is_empty()))
        .fold(PathBuf::from(root.as_ref()), |dir, c| dir.join(c))
        .to_string_lossy()
        .into_owned()
}

impl Location {
    /// The directories chosen, before any existence check.
    fn chosen(
        &self,
        toolchain: Toolchain,
        env: &impl Fn(&str) -> Option<String>,
        home: Option<&str>,
    ) -> Result<Vec<String>, PolicyError> {
        let Some((over, value)) = (self.overrides.iter()).find_map(|&o| Some((o, o.value(env)?)))
        else {
            return Ok(self
                .defaults
                .iter()
                .filter_map(|b| b.dir(env, home))
                .collect());
        };
        let dir = join(&value, over.sub());
        let subject = format!("{} is {value}", over.var());
        refuse_the_world(
            Selector::Toolchain(toolchain),
            &subject,
            over.var(),
            &dir,
            home,
        )?;
        Ok(vec![dir])
    }
}

/// The directories `toolchain` expands to over an injected environment, home
/// and `exists`.  A toolchain that is absent expands to nothing.
///
/// # Errors
/// An override that names the filesystem root, the home or an ancestor of it.
pub fn expand_in(
    toolchain: Toolchain,
    env: impl Fn(&str) -> Option<String>,
    home: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> Result<Vec<String>, PolicyError> {
    let chosen = (toolchain.locations().iter())
        .map(|l| l.chosen(toolchain, &env, home))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(chosen
        .into_iter()
        .flatten()
        .filter(|dir| exists(dir))
        .collect())
}

/// [`expand_in`] over the host environment and filesystem.
///
/// # Errors
/// As [`expand_in`].
pub fn expand(toolchain: Toolchain, home: Option<&str>) -> Result<Vec<String>, PolicyError> {
    expand_in(
        toolchain,
        |k| std::env::var(k).ok(),
        home,
        crate::path::exists,
    )
}

/// The [`FLOOR`] rows' directories outside `home`: a prefix an override puts
/// in the home is the user's, to be read only where granted.  A row that
/// fails is dropped.
pub(crate) fn floor_in(
    env: impl Fn(&str) -> Option<String>,
    home: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> Vec<String> {
    let held = home.map(FrozenPath::from_surface);
    let outside = |dir: &String| {
        held.as_ref()
            .is_none_or(|h| !h.contains::<Allow>(FrozenPath::from_surface(dir).real_path()))
    };
    FLOOR
        .into_iter()
        .filter_map(|t| expand_in(t, &env, home, &exists).ok())
        .flatten()
        .filter(outside)
        .collect()
}

/// [`floor_in`] over the host environment and filesystem.
pub(crate) fn floor(home: Option<&str>) -> Vec<String> {
    floor_in(|k| std::env::var(k).ok(), home, crate::path::exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::freeze::Anchor;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find_map(|&(var, v)| (var == k).then(|| v.to_owned()))
        }
    }

    fn all(_: &str) -> bool {
        true
    }

    fn none(_: &str) -> bool {
        false
    }

    fn only<'a>(present: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |p| present.contains(&p)
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

    /// Every row against an empty environment, `/h` and a full disk: the
    /// defaults, by anchor.
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
            assert_eq!(
                expand_in(t, env(&[]), Some("/h"), all).unwrap(),
                want,
                "{t:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_override_replaces_only_its_own_location() {
        let got = expand_in(
            Toolchain::Go,
            env(&[("GOROOT", "/opt/go")]),
            Some("/h"),
            all,
        );
        assert_eq!(got.unwrap(), ["/opt/go", "/h/go/bin"]);
        let got = expand_in(
            Toolchain::Deno,
            env(&[("DENO_INSTALL", "/d")]),
            Some("/h"),
            all,
        );
        assert_eq!(got.unwrap(), ["/d", "/h/.deno/bin"]);
    }

    #[cfg(unix)]
    #[test]
    fn overrides_are_tried_in_order() {
        let go = |pairs: &[(&str, &str)]| {
            let got = expand_in(Toolchain::Go, env(pairs), Some("/h"), all).unwrap();
            got[3..].to_vec()
        };
        assert_eq!(go(&[("GOBIN", "/b"), ("GOPATH", "/p")]), ["/b"]);
        assert_eq!(go(&[("GOPATH", "/p")]), ["/p/bin"]);
        let cargo = |pairs: &[(&str, &str)]| {
            expand_in(Toolchain::Cargo, env(pairs), Some("/h"), all).unwrap()
        };
        assert_eq!(
            cargo(&[("CARGO_INSTALL_ROOT", "/i"), ("CARGO_HOME", "/c")]),
            ["/i/bin"]
        );
        assert_eq!(cargo(&[("CARGO_HOME", "/c")]), ["/c/bin"]);
    }

    /// The environment said where the manager is: no fallback to defaults.
    #[cfg(unix)]
    #[test]
    fn a_chosen_override_naming_a_missing_directory_yields_nothing() {
        let got = expand_in(
            Toolchain::Cargo,
            env(&[("CARGO_HOME", "/c")]),
            Some("/h"),
            only(&["/h/.cargo/bin"]),
        );
        assert_eq!(got.unwrap(), Vec::<String>::new());
        let got = expand_in(
            Toolchain::Homebrew,
            env(&[("HOMEBREW_PREFIX", "/usr/local")]),
            Some("/h"),
            |p| p != "/usr/local",
        );
        assert_eq!(got.unwrap(), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_first_gopath_entry_counts() {
        let got = expand_in(
            Toolchain::Go,
            env(&[("GOPATH", "/p:/q")]),
            Some("/h"),
            only(&["/p/bin", "/q/bin", "/p:/q/bin"]),
        );
        assert_eq!(got.unwrap(), ["/p/bin"]);
    }

    #[cfg(unix)]
    #[test]
    fn empty_and_relative_values_are_ignored() {
        for bad in ["", "rel/c", "./c"] {
            let got = expand_in(
                Toolchain::Cargo,
                env(&[("CARGO_HOME", bad), ("CARGO_INSTALL_ROOT", bad)]),
                Some("/h"),
                all,
            );
            assert_eq!(got.unwrap(), ["/h/.cargo/bin"], "{bad:?}");
        }
        let got = expand_in(
            Toolchain::Homebrew,
            env(&[("HOMEBREW_PREFIX", "brew")]),
            None,
            only(&["/opt/homebrew"]),
        );
        assert_eq!(got.unwrap(), ["/opt/homebrew"]);
    }

    #[cfg(unix)]
    #[test]
    fn data_defaults_follow_xdg_data_home() {
        let got = expand_in(
            Toolchain::Fnm,
            env(&[("XDG_DATA_HOME", "/x")]),
            Some("/h"),
            all,
        );
        assert_eq!(
            got.unwrap(),
            ["/x/fnm", "/h/.fnm", "/h/Library/Application Support/fnm"]
        );
        let got = expand_in(
            Toolchain::Fnm,
            env(&[("XDG_DATA_HOME", "rel")]),
            Some("/h"),
            only(&["/h/.local/share/fnm"]),
        );
        assert_eq!(got.unwrap(), ["/h/.local/share/fnm"]);
    }

    /// A selector reports what is installed: with no home, home defaults are
    /// absent, and an absolute `XDG_*_HOME` alone still answers.
    #[cfg(unix)]
    #[test]
    fn an_unbound_home_leaves_home_defaults_absent() {
        let expand = |t, pairs: &[(&str, &str)]| expand_in(t, env(pairs), None, all).unwrap();
        assert_eq!(expand(Toolchain::Rustup, &[]), Vec::<String>::new());
        assert_eq!(expand(Toolchain::Fnm, &[]), Vec::<String>::new());
        assert_eq!(
            expand(Toolchain::Fnm, &[("XDG_DATA_HOME", "/x")]),
            ["/x/fnm"]
        );
        assert_eq!(expand(Toolchain::Nix, &[]), ["/nix/store"]);
        let got = expand_in(Toolchain::Rustup, env(&[]), Some("rel"), all);
        assert_eq!(got.unwrap(), Vec::<String>::new());
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
            assert_eq!(
                expand_in(t, env(&[]), None, all).unwrap(),
                Vec::<String>::new(),
                "{t:?}"
            );
        }
        for &t in native {
            assert!(
                !expand_in(t, env(&[]), None, all).unwrap().is_empty(),
                "{t:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_override_naming_the_world_is_refused() {
        for (home, value, what) in [
            ("/h", "/", "the filesystem root"),
            ("/h", "/h", "the home directory itself"),
            ("/h", "/h/../h", "the home directory itself"),
            ("/home/a", "/home", "an ancestor of the home directory"),
        ] {
            let err = expand_in(
                Toolchain::Rustup,
                env(&[("RUSTUP_HOME", value)]),
                Some(home),
                all,
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
        let expand = |t, var, v| expand_in(t, env(&[(var, v)]), Some("/h"), all).unwrap();
        assert_eq!(
            expand(Toolchain::Ghcup, "GHCUP_INSTALL_BASE_PREFIX", "/h"),
            ["/h/.ghcup"]
        );
        assert_eq!(expand(Toolchain::Go, "GOPATH", "/h")[3..], ["/h/bin"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_floor_holds_the_system_prefixes_and_not_a_prefix_in_the_home() {
        let floor = |pairs: &[(&str, &str)], present: &[&str]| {
            floor_in(env(pairs), Some("/h"), only(present))
        };
        let prefixes = [
            "/opt/homebrew",
            "/opt/local",
            "/nix/store",
            "/usr/pkg",
            "/opt/pkg",
        ];
        assert_eq!(floor(&[], &prefixes), prefixes);
        assert_eq!(floor(&[], &[]), Vec::<String>::new());
        let home_brew = [("HOMEBREW_PREFIX", "/h/linuxbrew")];
        assert_eq!(
            floor(&home_brew, &["/h/linuxbrew", "/opt/local"]),
            ["/opt/local"]
        );
        let elsewhere = [("HOMEBREW_PREFIX", "/usr/local")];
        assert_eq!(floor(&elsewhere, &["/usr/local"]), ["/usr/local"]);
        // A row that fails is dropped, the others stay.
        let world = [("HOMEBREW_PREFIX", "/")];
        assert_eq!(floor(&world, &["/", "/opt/local"]), ["/opt/local"]);
    }

    #[cfg(unix)]
    #[test]
    fn system_is_kept_by_existence_and_carries_no_homebrew() {
        let system = |exists: &dyn Fn(&str) -> bool| {
            expand_in(Toolchain::System, env(&[]), None, exists).unwrap()
        };
        assert_eq!(system(&none), Vec::<String>::new());
        assert_eq!(system(&|p| p.contains("brew")), Vec::<String>::new());
        let tools = [
            "/usr/libexec",
            "/usr/lib/git-core",
            "/usr/lib/gcc",
            "/Library/Developer/CommandLineTools",
            "/Applications/Xcode.app/Contents/Developer",
        ];
        assert_eq!(system(&|p| tools.contains(&p)), tools);
        for present in tools.into_iter().chain(["/usr/bin", "/bin"]) {
            assert_eq!(system(&|p| p == present), [present]);
        }
    }

    /// Rustup resolves `cargo` under `~/.rustup/toolchains/`, a path shape
    /// easy to miss when hand-enumerating exec roots.
    #[cfg(unix)]
    #[test]
    fn rustup_covers_a_toolchain_cargo() {
        let roots = expand_in(Toolchain::Rustup, env(&[]), Some("/h"), all).unwrap();
        let cargo =
            FrozenPath::from_surface("/h/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo");
        assert!(
            roots
                .iter()
                .any(|r| FrozenPath::from_surface(r).contains::<Allow>(cargo.real_path())),
            "{roots:?}"
        );
    }

    #[test]
    fn an_absent_manager_expands_to_nothing() {
        for &t in Toolchain::VARIANTS {
            let got = expand_in(t, env(&[]), Some("/h"), none);
            assert_eq!(got.unwrap(), Vec::<String>::new(), "{t:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn system_on_windows() {
        let system = |pairs: &[(&str, &str)], exists: &dyn Fn(&str) -> bool| {
            expand_in(Toolchain::System, env(pairs), None, exists).unwrap()
        };
        let root = [("SystemRoot", r"C:\Windows")];
        assert_eq!(
            system(&root, &|p| p.starts_with(r"C:\Windows")),
            [
                r"C:\Windows\System32",
                r"C:\Windows\System32\WindowsPowerShell\v1.0"
            ]
        );
        assert_eq!(
            system(&[], &|p| p.starts_with(r"C:\Windows")),
            [
                r"C:\Windows\System32",
                r"C:\Windows\System32\WindowsPowerShell\v1.0"
            ]
        );
        let git = r"C:\Program Files\Git\usr\bin";
        let pf = [("ProgramFiles", r"C:\Program Files")];
        assert_eq!(system(&pf, &|p| p == git), [git]);
        assert_eq!(system(&pf, &none), Vec::<String>::new());
        let x86 = [("ProgramFiles(x86)", r"C:\Program Files (x86)")];
        let git86 = r"C:\Program Files (x86)\Git\usr\bin";
        assert_eq!(system(&x86, &|p| p == git86), [git86]);
    }
}
