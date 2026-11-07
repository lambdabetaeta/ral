//! The directories the host chooses: the package managers a `dir` selector
//! names, as one table, and the system reads beside them.
//!
//! A [`Toolchain`] is a set of [`Location`]s, and so is [`LISTED`], the
//! system reads no selector names.  A location's directories are chosen from
//! the host environment alone: the first usable [`Override`] chooses and is
//! all the location has, else every [`Base`] default is chosen.  An override
//! naming a missing directory yields nothing, since the environment disowned
//! the defaults.  The rules are stated once, in `docs/ral-wiki/design/grant.md`
//! §Selectors; the table is the data.
//!
//! [`chosen`] is pure over an injected environment and home.  [`walked`]
//! holds each chosen directory as [`host_absolute`] does, refusing a malformed
//! one, and walks it.  A `dir` selector keeps the directories, refusing one
//! that names the world ([`expand_in`]; `expand`, under `test-util`, is its live wrapper); the
//! system reads keep what exists outside the world and the home
//! ([`system_reads_in`]).

use super::Polar;
use super::freeze::{Home, PolicyError, Selector, host_absolute, refuse_the_world, the_world};
use crate::authority::policy::GrantPath;
use crate::path::{Kind, RealPath, Trace, is_absolute, join_slash, path_entries, shown};
use crate::path::{XdgKind, resolve_xdg_in};
use crate::text::code;
use std::path::{Path, PathBuf};
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
/// standing of `/usr`, so system reads include them.
pub const SYSTEM_READS: [Toolchain; 4] = [
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
                        Abs("/usr/lib/cargo/bin"),
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

/// The system reads no selector names, beside the rows of [`SYSTEM_READS`]:
/// what every program on this platform reads to start.  One may be a file
/// (`/etc/passwd`).  Every platform's table is taken on every host, so that a
/// test can judge any.
const LISTED: &[Location] = if cfg!(windows) {
    WINDOWS_LISTED
} else if cfg!(target_os = "linux") {
    LINUX_LISTED
} else if cfg!(target_os = "macos") {
    MACOS_LISTED
} else {
    &[]
};

const LINUX_LISTED: &[Location] = {
    use Base::Abs;
    &[loc!(
        [],
        [
            Abs("/bin"),
            Abs("/usr"),
            Abs("/lib"),
            Abs("/lib64"),
            // `/dev`, `/proc` and `/sys/fs/cgroup` are absent: the envelope
            // mounts its own.  Of `/etc` and `/sys`, only what dynamic
            // linking, name resolution, user lookup, toolchain resolution and
            // hardware sizing need: the rest of `/sys` describes the host, not
            // the envelope (`class/net` is the mounter's netns whatever
            // `--unshare-net` did; `class/dmi` and `bus` name the machine).
            Abs("/sys/devices/system/cpu"),
            Abs("/sys/kernel/mm/transparent_hugepage"),
            Abs("/etc/ld.so.conf"),
            Abs("/etc/ld.so.conf.d"),
            Abs("/etc/ld.so.cache"),
            Abs("/etc/resolv.conf"),
            Abs("/etc/nsswitch.conf"),
            Abs("/etc/hosts"),
            Abs("/etc/ssl"),
            Abs("/etc/ca-certificates"),
            Abs("/etc/pki"),
            // getpwuid/getgrgid sit in libc startup paths: without these many
            // programs cannot even resolve HOME.
            Abs("/etc/passwd"),
            Abs("/etc/group"),
            // Debian/Ubuntu toolchain symlinks (cc → gcc-13, etc.).
            Abs("/etc/alternatives")
        ]
    )]
};

const MACOS_LISTED: &[Location] = {
    use Base::Abs;
    &[loc!(
        [],
        [
            Abs("/bin"),
            Abs("/usr"),
            // Rosetta's runtime, the loader analogue: no grant should name it.
            Abs("/Library/Apple/usr"),
            Abs("/Library/Developer/CommandLineTools"),
            Abs("/Applications/Xcode.app/Contents/Developer"),
            Abs("/lib"),
            Abs("/System"),
            Abs("/dev"),
            Abs("/private/var/db/dyld"),
            // Wholesale: tools read whatever they read (gitconfig, paths.d,
            // zshenv, nix.conf, …); nothing user-secret sits here unprotected.
            Abs("/private/etc"),
            // xcode-select state; denied, build drivers report a broken install.
            Abs("/private/var/select"),
            // resolv.conf's target and the mDNSResponder socket.
            Abs("/private/var/run")
        ]
    )]
};

/// `%SystemRoot%`, `%ProgramFiles%` and `%ProgramFiles(x86)%`: an unset
/// variable lists nothing.
const WINDOWS_LISTED: &[Location] = {
    use Override::Var;
    &[
        loc!([Var("SystemRoot", "")], []),
        loc!([Var("ProgramFiles", "")], []),
        loc!([Var("ProgramFiles(x86)", "")], []),
    ]
};

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

/// The directories `locations` choose over an injected environment and
/// home, native as the OS takes them, before any is walked.
pub(super) fn chosen<'a>(
    locations: impl IntoIterator<Item = &'a Location>,
    env: impl Fn(&str) -> Option<String>,
    home: Option<&str>,
) -> Vec<Chosen> {
    (locations.into_iter())
        .flat_map(|l| l.chosen(&env, home))
        .collect()
}

/// Each directory `locations` choose, held as [`host_absolute`] holds a path
/// and walked as `W` walks, beside the override that chose it; one that is
/// absent is dropped.  `holder` names what would hold it, for a refusal.
///
/// # Errors
/// A chosen directory [`host_absolute`] refuses, named with what chose it;
/// one whose walk fails.
fn walked<'a, W: Polar>(
    locations: impl IntoIterator<Item = &'a Location>,
    holder: &str,
    env: impl Fn(&str) -> Option<String>,
    home: Option<&str>,
) -> Result<Vec<Found<W>>, PolicyError> {
    (chosen(locations, env, home).into_iter())
        .filter_map(|Chosen { dir, via }| {
            host_absolute(&dir)
                .map_err(|why| malformed(&dir, via.as_ref(), holder, &why))
                .and_then(GrantPath::<W>::walked)
                .map(|w| w.real.followed().kind().is_some().then_some((w, via)))
                .transpose()
        })
        .collect()
}

/// A chosen `dir` that `holder` cannot hold, [`host_absolute`] having
/// refused it for `why`: what chose it, and what to change.
fn malformed(dir: &Path, via: Option<&Via>, holder: &str, why: &PolicyError) -> PolicyError {
    let (subject, fix) = match via {
        Some((var, value)) => (
            format!("{var} is {}", shown(value)),
            format!("Set {var} to a well-formed absolute path, or unset it."),
        ),
        None => (
            format!("default directory {}", shown(dir)),
            "Check its HOME/XDG base.".to_owned(),
        ),
    };
    PolicyError::new(format!(
        "{subject}, which {holder} cannot hold: {}",
        why.message
    ))
    .with_hint(fix)
}

/// The directories `toolchain` expands to over an injected environment and
/// home, each walked as polarity `W` walks: one that is malformed or names a
/// filesystem root, the home or an ancestor of it is refused, and one that is
/// absent or no directory is dropped.  A toolchain that is absent expands to
/// nothing.
///
/// # Errors
/// A selected directory that is malformed or names the world; one whose walk
/// fails, or that no grant can hold.
pub(crate) fn expand_in<W: Polar>(
    toolchain: Toolchain,
    env: impl Fn(&str) -> Option<String>,
    home: Option<&Home>,
) -> Result<Vec<GrantPath<W::Grant>>, PolicyError> {
    let selector = Selector::Toolchain(toolchain);
    let holder = code(&format!("`dir `{}", selector.name()));
    let spelled = home.map(|h| h.spelled.as_str());
    walked::<W>(toolchain.locations(), &holder, env, spelled)?
        .into_iter()
        .filter(|(dir, _)| dir.real.followed().kind() == Some(Kind::Dir))
        .map(|(dir, via)| {
            let (subject, advice) = provenance(&dir, via);
            refuse_the_world(selector, &subject, &advice, dir.real.followed(), home)?;
            W::grant(dir)
        })
        .collect()
}

/// What chose `dir`, and what to check, for a refusal.
fn provenance<W: Polar>(dir: &GrantPath<W>, via: Option<Via>) -> (String, String) {
    if let Some((var, value)) = via {
        (
            format!("{var} is {}", shown(&value)),
            format!("Is {var} set correctly?"),
        )
    } else {
        let written = dir.written.shown();
        let real = dir.real.followed().real().to_string();
        let subject = if written == real {
            format!("default directory {written}")
        } else {
            format!("default directory {written} resolves to {real}")
        };
        (
            subject,
            "Check the default directory or symlink and its HOME/XDG base.".into(),
        )
    }
}

/// [`expand_in`] over the host environment and filesystem, as the spellings
/// that survive.
///
/// # Errors
/// As [`expand_in`]; a home that cannot be walked.
#[cfg(any(test, feature = "test-util"))]
pub fn expand(toolchain: Toolchain, home: Option<&str>) -> Result<Vec<PathBuf>, PolicyError> {
    let home = home.map(Home::of).transpose()?;
    let kept = expand_in::<Trace>(toolchain, crate::host::var, home.as_ref())?;
    Ok(kept
        .iter()
        .map(|dir| PathBuf::from(dir.written.as_str()))
        .collect())
}

/// System reads exclude roots, the home, its ancestors and its contents.
pub(crate) fn keeps_system_read(dir: &RealPath, held: Option<&RealPath>) -> bool {
    the_world(dir, held).is_none() && held.is_none_or(|h| !h.contains(dir))
}

/// The host's system reads over an injected environment and home: what
/// [`LISTED`] and the rows of [`SYSTEM_READS`] choose.
///
/// # Errors
/// As [`system_reads`].
pub(crate) fn system_reads_in(
    env: impl Fn(&str) -> Option<String>,
    home: Option<&Home>,
) -> Result<Vec<GrantPath<Trace>>, PolicyError> {
    let rows = SYSTEM_READS.iter().flat_map(|t| t.locations());
    system_reads(LISTED.iter().chain(rows), env, home)
}

/// What `locations` choose that exists and [`keeps_system_read`]; the rest is
/// dropped, never granted.
///
/// # Errors
/// A chosen directory [`host_absolute`] refuses, named with what chose it;
/// one whose walk fails.
fn system_reads<'a>(
    locations: impl IntoIterator<Item = &'a Location>,
    env: impl Fn(&str) -> Option<String>,
    home: Option<&Home>,
) -> Result<Vec<GrantPath<Trace>>, PolicyError> {
    let held = home.and_then(|h| h.real.as_ref());
    let holder = "ral's read floor (the system paths every program may read)";
    let found = walked::<Trace>(locations, holder, env, home.map(|h| h.spelled.as_str()))?;
    Ok((found.into_iter())
        .map(|(dir, _)| dir)
        .filter(|dir| keeps_system_read(dir.real.real(), held))
        .collect())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::grant::freeze::Anchor;
    use crate::path::{Depth, Excluded, Subject};
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
        (chosen(toolchain.locations(), env, home).iter())
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

    /// [`system_reads`] over the rows of [`SYSTEM_READS`], as the spellings
    /// it keeps.
    #[cfg(unix)]
    fn system_reads_of(env: impl Fn(&str) -> Option<String>, home: &str) -> Vec<String> {
        let home = Home::of(home).unwrap();
        let rows = SYSTEM_READS.iter().flat_map(|t| t.locations());
        let dirs = system_reads(rows, env, Some(&home)).unwrap();
        dirs.iter().map(|d| d.written.as_str().to_owned()).collect()
    }

    /// [`system_reads`] over `locations`, as the spellings it keeps.
    fn kept(locations: &[Location], pairs: &[(&str, &str)], home: Option<&Home>) -> Vec<String> {
        let dirs = system_reads(locations, env(pairs), home).unwrap();
        dirs.iter().map(|d| d.written.as_str().to_owned()).collect()
    }

    /// Four locations, each chosen by its own variable alone: a fixture's
    /// listed system reads.
    const PROBE: &[Location] = {
        use Override::Var;
        &[
            loc!([Var("A", "")], []),
            loc!([Var("B", "")], []),
            loc!([Var("C", "")], []),
            loc!([Var("D", "")], []),
        ]
    };

    /// The root of this host's filesystem, as a variable spells it.
    fn host_root() -> &'static str {
        if cfg!(windows) { r"C:\" } else { "/" }
    }

    /// A path absolute to the OS that `absolute` refuses: a malformed UNC head.
    fn malformed_dir() -> &'static str {
        if cfg!(windows) {
            r"\\\x\bin"
        } else {
            "///x/bin"
        }
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

    fn link_dir(target: &Path, at: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, at).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(target, at).unwrap();
    }

    fn default_refusal<W: Polar>(
        toolchain: Toolchain,
        pairs: &[(&str, &str)],
        home: &Home,
        default: &Path,
        target: &Path,
        what: &str,
    ) {
        let err = expand_in::<W>(toolchain, env(pairs), Some(home))
            .err()
            .expect("a world-sized default must refuse the selector");
        for part in [
            format!("`dir `{}", <&str>::from(toolchain)),
            format!("default directory {}", shown(default)),
            format!("resolves to {}", crate::test_helper::real(target)),
            what.to_owned(),
            "default directory or symlink".to_owned(),
            "HOME/XDG base".to_owned(),
        ] {
            assert!(err.message.contains(&part), "{part}: {}", err.message);
        }
        assert!(!err.message.contains("set correctly"), "{}", err.message);
    }

    #[test]
    fn rustup_defaults_naming_a_root_home_or_ancestor_refuse_both_polarities() {
        type Pick = fn(&Path, &Path) -> PathBuf;
        let pickers: [(&str, Pick); 3] = [
            ("the filesystem root", |tmp, _| {
                tmp.ancestors().last().unwrap().to_owned()
            }),
            ("the home directory itself", |_, h| h.to_owned()),
            ("an ancestor of the home directory", |tmp, _| tmp.to_owned()),
        ];
        for (what, pick) in pickers {
            let (tmp, _) = tree(&["h"]);
            let home_path = tmp.path().join("h");
            let home = Home::of(spelled(&home_path)).unwrap();
            let target = pick(tmp.path(), &home_path);
            let default = home_path.join(".rustup");
            link_dir(&target, &default);
            default_refusal::<Trace>(Toolchain::Rustup, &[], &home, &default, &target, what);
            default_refusal::<Excluded>(Toolchain::Rustup, &[], &home, &default, &target, what);
        }
    }

    #[test]
    fn xdg_config_data_and_cache_defaults_refuse_the_world_with_or_without_host_bases() {
        for (toolchain, var, fallback, sub) in [
            (Toolchain::Nvm, "XDG_CONFIG_HOME", ".config", "nvm"),
            (Toolchain::Fnm, "XDG_DATA_HOME", ".local/share", "fnm"),
            (
                Toolchain::Poetry,
                "XDG_CACHE_HOME",
                ".cache",
                "pypoetry/virtualenvs",
            ),
        ] {
            for injected in [false, true] {
                for at_root in [false, true] {
                    let (tmp, _) = tree(&["h/.nvm", "h/.local/share/pypoetry", "xdg"]);
                    let home_path = tmp.path().join("h");
                    let home = Home::of(spelled(&home_path)).unwrap();
                    let base = if injected {
                        tmp.path().join("xdg")
                    } else {
                        home_path.join(fallback)
                    };
                    let default = base.join(sub);
                    std::fs::create_dir_all(default.parent().unwrap()).unwrap();
                    let target = if at_root {
                        tmp.path().ancestors().last().unwrap()
                    } else {
                        home_path.as_path()
                    };
                    link_dir(target, &default);
                    let pairs = if injected {
                        vec![(var, spelled(&base))]
                    } else {
                        vec![]
                    };
                    let what = if at_root {
                        "the filesystem root"
                    } else {
                        "the home directory itself"
                    };
                    default_refusal::<Trace>(toolchain, &pairs, &home, &default, target, what);
                    default_refusal::<Excluded>(toolchain, &pairs, &home, &default, target, what);
                }
            }
        }
    }

    #[test]
    fn fnm_checks_an_unsafe_later_default_after_a_safe_data_default() {
        let (tmp, _) = tree(&["h/.local/share/fnm"]);
        let home_path = tmp.path().join("h");
        let home = Home::of(spelled(&home_path)).unwrap();
        let default = home_path.join(".fnm");
        link_dir(&home_path, &default);
        default_refusal::<Trace>(
            Toolchain::Fnm,
            &[],
            &home,
            &default,
            &home_path,
            "the home directory itself",
        );
        default_refusal::<Excluded>(
            Toolchain::Fnm,
            &[],
            &home,
            &default,
            &home_path,
            "the home directory itself",
        );
    }

    #[test]
    fn a_safe_external_default_grants_only_its_position_and_keeps_exclusion_objects() {
        let (tmp, _) = tree(&["h", "tools", "neighbor"]);
        let home_path = tmp.path().join("h");
        let home = Home::of(spelled(&home_path)).unwrap();
        let default = home_path.join(".rustup");
        let target = tmp.path().join("tools");
        link_dir(&target, &default);
        let allowed = expand_in::<Trace>(Toolchain::Rustup, env(&[]), Some(&home)).unwrap();
        let excluded = expand_in::<Excluded>(Toolchain::Rustup, env(&[]), Some(&home)).unwrap();
        assert_eq!(allowed.len(), 1);
        assert_eq!(excluded.len(), 1);
        assert_eq!(
            allowed[0].real.real(),
            Some(&crate::test_helper::real(&target))
        );
        let holds: Vec<_> = (excluded[0].real.holds())
            .map(crate::path::Hold::trace)
            .collect();
        let [named, linked] = &holds[..] else {
            panic!("the name and its link's target: {holds:?}");
        };
        assert_eq!(named.kind(), Some(Kind::Symlink));
        assert_eq!(linked.real(), &crate::test_helper::real(&target));
        #[cfg(unix)]
        {
            let ids = excluded[0].real.held_ids();
            assert_eq!(ids.len(), 2);
            assert!(ids.iter().all(Option::is_some), "{ids:?}");
        }
        for (path, covered) in [
            (target.join("cargo"), true),
            (default.join("cargo"), true),
            (tmp.path().join("neighbor/cargo"), false),
        ] {
            let access = crate::test_helper::trace(path);
            assert_eq!(allowed[0].real.contains(Subject::Access(&access)), covered);
            assert_eq!(
                excluded[0]
                    .real
                    .matches(Subject::Access(&access), Depth::Within)
                    .is_some(),
                covered
            );
        }
    }

    #[test]
    fn missing_and_non_directory_defaults_yield_nothing_for_both_polarities() {
        let (tmp, _) = tree(&["h"]);
        let home_path = tmp.path().join("h");
        let home = Home::of(spelled(&home_path)).unwrap();
        let empty = || {
            assert_eq!(
                expand_in::<Trace>(Toolchain::Rustup, env(&[]), Some(&home)).unwrap(),
                vec![]
            );
            assert_eq!(
                expand_in::<Excluded>(Toolchain::Rustup, env(&[]), Some(&home)).unwrap(),
                vec![]
            );
        };
        empty();
        std::fs::write(home_path.join(".rustup"), "not a directory").unwrap();
        empty();
    }

    #[test]
    fn empty_or_relative_overrides_still_judge_the_default() {
        let (tmp, _) = tree(&["h"]);
        let home_path = tmp.path().join("h");
        let home = Home::of(spelled(&home_path)).unwrap();
        let default = home_path.join(".rustup");
        link_dir(&home_path, &default);
        for value in ["", "relative"] {
            let pairs = [("RUSTUP_HOME", value)];
            default_refusal::<Trace>(
                Toolchain::Rustup,
                &pairs,
                &home,
                &default,
                &home_path,
                "the home directory itself",
            );
            default_refusal::<Excluded>(
                Toolchain::Rustup,
                &pairs,
                &home,
                &default,
                &home_path,
                "the home directory itself",
            );
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
    fn the_system_reads_holds_the_system_prefixes_and_not_a_prefix_in_the_home() {
        let (_tmp, root) = tree(&["h/linuxbrew", "elsewhere"]);
        let home = format!("{root}/h");
        let baseline = |pairs: &[(&str, &str)]| system_reads_of(env(pairs), &home);
        let prefixes = [
            "/opt/homebrew",
            "/home/linuxbrew/.linuxbrew",
            "/opt/local",
            "/nix/store",
            "/usr/pkg",
            "/opt/pkg",
        ];
        let held = baseline(&[]);
        assert!(
            held.iter().all(|p| prefixes.contains(&p.as_str())),
            "{held:?}"
        );
        // What the other rows hold on this host, the homebrew row disowned.
        let gone = format!("{root}/gone");
        let rest = baseline(&[("HOMEBREW_PREFIX", &gone)]);
        let elsewhere = format!("{root}/elsewhere");
        assert_eq!(
            baseline(&[("HOMEBREW_PREFIX", &elsewhere)]),
            [vec![elsewhere.clone()], rest.clone()].concat()
        );
        let in_home = format!("{home}/linuxbrew");
        assert_eq!(baseline(&[("HOMEBREW_PREFIX", &in_home)]), rest);
        // A row that fails is dropped, the others stay.
        assert_eq!(baseline(&[("HOMEBREW_PREFIX", "/")]), rest);
    }

    #[cfg(unix)]
    #[test]
    fn a_system_reads_directory_whose_walk_fails_refuses_the_system_reads() {
        let (_tmp, root) = tree(&[]);
        let looped = format!("{root}/loop");
        std::os::unix::fs::symlink("loop", &looped).unwrap();
        let err = system_reads_in(env(&[("HOMEBREW_PREFIX", &looped)]), None).unwrap_err();
        assert!(err.message.contains(&looped), "{}", err.message);
    }

    #[test]
    fn an_override_naming_the_world_or_the_home_is_no_system_reads_and_no_error() {
        let (tmp, root) = tree(&["h"]);
        let home_dir = spelled(&tmp.path().join("h")).to_owned();
        let home = Home::of(&home_dir).unwrap();
        let held = home.real.clone().unwrap();
        let kept = |prefix: &str, home: Option<&Home>| {
            system_reads_in(env(&[("HOMEBREW_PREFIX", prefix)]), home).unwrap()
        };
        let host_root = if cfg!(windows) { r"C:\" } else { "/" };
        for home in [None, Some(&home)] {
            let baseline = kept(host_root, home);
            assert!(
                baseline.iter().all(|d| !d.real.real().is_root()),
                "{baseline:?}"
            );
        }
        for prefix in [home_dir.as_str(), root.as_str()] {
            let baseline = kept(prefix, Some(&home));
            assert!(
                baseline
                    .iter()
                    .all(|d| !held.contains(d.real.real()) && !d.real.real().contains(&held)),
                "{prefix}: {baseline:?}"
            );
        }
    }

    /// The baseline's one rule, on directories as walked.
    #[test]
    fn the_keeps_system_read_neither_the_world_nor_the_home_nor_what_lies_in_it() {
        let (tmp, _) = tree(&["h/in", "beside"]);
        let at = |sub: &str| crate::test_helper::real(tmp.path().join(sub));
        let (top, home, inside, beside) = (
            crate::test_helper::real(tmp.path()),
            at("h"),
            at("h/in"),
            at("beside"),
        );
        let host_root = crate::test_helper::real(if cfg!(windows) { r"C:\" } else { "/" });
        for held in [None, Some(&home)] {
            assert!(!keeps_system_read(&host_root, held), "a root: {held:?}");
            assert!(
                keeps_system_read(&beside, held),
                "beside the home: {held:?}"
            );
        }
        assert!(
            keeps_system_read(&home, None),
            "no home to be the world or inside"
        );
        for dir in [&home, &inside, &top] {
            assert!(!keeps_system_read(dir, Some(&home)), "{dir:?}");
        }
    }

    /// A listed directory that is a filesystem root is no system read and no
    /// error, as a toolchain row's is not: the root would hold the whole
    /// filesystem.
    #[test]
    fn a_listed_root_is_dropped_and_an_ordinary_directory_kept() {
        let (tmp, _) = tree(&["ordinary"]);
        let ordinary = spelled(&tmp.path().join("ordinary")).to_owned();
        let pairs = [("A", host_root()), ("B", ordinary.as_str())];
        assert_eq!(kept(PROBE, &pairs, None), [ordinary]);
    }

    /// The home, what lies inside it and what it lies inside are the user's
    /// to grant or the world: a listed directory so placed is dropped, and
    /// one beside the home is kept.
    #[test]
    fn a_listed_directory_in_the_home_or_around_it_is_dropped() {
        let (tmp, top) = tree(&["h/inside", "beside"]);
        let home_dir = tmp.path().join("h");
        let (h, inside, beside) = (
            spelled(&home_dir).to_owned(),
            spelled(&home_dir.join("inside")).to_owned(),
            spelled(&tmp.path().join("beside")).to_owned(),
        );
        let home = Home::of(&h).unwrap();
        assert!(home.real.is_some(), "an absolute home lies somewhere");
        let pairs = [
            ("A", top.as_str()),
            ("B", h.as_str()),
            ("C", inside.as_str()),
            ("D", beside.as_str()),
        ];
        assert_eq!(kept(PROBE, &pairs, Some(&home)), [beside]);
    }

    /// Windows' system directories are its listed system reads, judged on any
    /// host: a variable naming a root puts no root among them, and one unset
    /// lists nothing.
    #[test]
    fn a_windows_system_variable_naming_a_root_is_dropped() {
        let (tmp, _) = tree(&["programs"]);
        let programs = spelled(&tmp.path().join("programs")).to_owned();
        let pairs = [
            ("SystemRoot", host_root()),
            ("ProgramFiles", programs.as_str()),
        ];
        assert_eq!(kept(WINDOWS_LISTED, &pairs, None), [programs]);
    }

    /// A system read is whatever exists, a file included (`/etc/passwd` is
    /// one); a `dir` selector keeps only directories.
    #[test]
    fn a_system_read_may_be_a_file_and_a_selector_directory_may_not() {
        let (tmp, _) = tree(&[]);
        let file = tmp.path().join("passwd");
        std::fs::write(&file, "").unwrap();
        let file = spelled(&file).to_owned();
        assert_eq!(
            kept(PROBE, &[("A", file.as_str())], None),
            std::slice::from_ref(&file)
        );
        let got = expanded(
            Toolchain::Rustup,
            env(&[("RUSTUP_HOME", file.as_str())]),
            None,
        );
        assert_eq!(got.unwrap(), Vec::<String>::new());
    }

    /// A chosen directory absolute to the OS but one `absolute` refuses is
    /// held by no selector: the selector refuses, naming the variable, itself
    /// and why, and says what to change.
    #[test]
    fn a_malformed_override_refuses_the_selector_naming_it() {
        let value = malformed_dir();
        let err = expanded(Toolchain::Rustup, env(&[("RUSTUP_HOME", value)]), None).unwrap_err();
        for part in [
            format!("RUSTUP_HOME is {}, which", shown(value)),
            "`dir `rustup".to_owned(),
            "no well-formed UNC".to_owned(),
        ] {
            assert!(err.message.contains(&part), "{part}: {}", err.message);
        }
        assert_eq!(
            err.hint.as_deref(),
            Some("Set RUSTUP_HOME to a well-formed absolute path, or unset it.")
        );
    }

    /// The same of a default, which names no variable.
    #[cfg(unix)]
    #[test]
    fn a_malformed_default_refuses_naming_the_default() {
        const MALFORMED: &[Location] = &[loc!([], [Base::Abs("///x/bin")])];
        let err = walked::<Trace>(MALFORMED, "a fixture", env(&[]), None).unwrap_err();
        assert!(
            err.message
                .starts_with("default directory ///x/bin, which a fixture cannot hold: "),
            "{}",
            err.message
        );
        assert!(err.hint.is_some_and(|h| h.contains("HOME/XDG")));
    }

    /// The system reads refuse a malformed choice as a selector does, naming
    /// the variable and the read floor: no system read is dropped for being
    /// malformed.
    #[test]
    fn a_malformed_system_read_refuses_the_system_reads() {
        let value = malformed_dir();
        let err = system_reads(PROBE, env(&[("A", value)]), None).unwrap_err();
        assert!(
            err.message
                .starts_with(&format!("A is {}, which ral's read floor", shown(value))),
            "{}",
            err.message
        );
        assert_eq!(
            err.hint.as_deref(),
            Some("Set A to a well-formed absolute path, or unset it.")
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_defaults_include_distribution_tool_roots() {
        let system = chosen_in(Toolchain::System, env(&[]), None);
        assert!(system.iter().any(|dir| dir == "/usr/lib/cargo/bin"));
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
