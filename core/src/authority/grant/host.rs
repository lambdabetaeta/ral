//! Discover and resolve the host's system reads.

use super::freeze::Home;
use super::toolchains;
use crate::authority::policy::{Baseline, FsAllow, GrantPath};
use crate::path::{FrozenPath, LeafMode, RealPath, walk_lenient};

/// Rosetta's runtime, the loader analogue: always readable.
#[cfg(target_os = "macos")]
const ROSETTA: &str = "/Library/Apple/usr";

/// Windows' system-read directories, as `env` names them:
/// `%SystemRoot%`, `%ProgramFiles%` and `%ProgramFiles(x86)%`.  A variable
/// that is unset is the empty spelling, which [`resolve_reads`] drops as not
/// absolute.  Taken on every host, so that a test can judge it on any.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_listed(env: impl Fn(&str) -> Option<String>) -> [String; 3] {
    ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)"].map(|k| env(k).unwrap_or_default())
}

impl Baseline {
    /// [`of_host`](Self::of_host) with the process's home given, by which
    /// every directory of the baseline, listed or a toolchain row's, is judged:
    /// [`toolchains::keeps_system_read`] drops a root, the home, an ancestor of it
    /// and what lies inside it.
    ///
    /// # Errors
    /// As [`of_host`](Self::of_host); a `home` that cannot be walked, named.
    pub(crate) fn of_home(home: Option<&str>) -> Result<Self, String> {
        Self::of_env(crate::host::var, home)
    }

    /// [`of_home`](Self::of_home) over an injected environment, which names
    /// Windows' system directories and the toolchain rows' overrides.  The
    /// home is walked once, and judges every directory the baseline holds, the
    /// listed and the toolchain rows': one rule, [`toolchains::keeps_system_read`].
    ///
    /// # Errors
    /// As [`of_home`](Self::of_home).
    pub(crate) fn of_env(
        env: impl Fn(&str) -> Option<String>,
        home: Option<&str>,
    ) -> Result<Self, String> {
        let home = home.map(Home::of).transpose().map_err(|e| e.message)?;
        #[cfg(windows)]
        let listed = windows_listed(&env);
        #[cfg(target_os = "linux")]
        let listed = [
            "/bin",
            "/usr",
            "/lib",
            "/lib64",
            // `/dev`, `/proc` and `/sys/fs/cgroup` are absent: the envelope
            // mounts its own.  `/etc` and `/sys` wholesale are excluded: only what dynamic
            // linking, name resolution, user lookup, toolchain resolution and
            // hardware sizing need.  The rest of `/sys` describes the host,
            // not the envelope: `class/net` is the mounter's netns whatever
            // `--unshare-net` did, `class/dmi` and `bus` name the machine.  A
            // grant that wants it reads `/sys` by name.
            "/sys/devices/system/cpu",
            "/sys/kernel/mm/transparent_hugepage",
            "/etc/ld.so.conf",
            "/etc/ld.so.conf.d",
            "/etc/ld.so.cache",
            "/etc/resolv.conf",
            "/etc/nsswitch.conf",
            "/etc/hosts",
            "/etc/ssl",
            "/etc/ca-certificates",
            "/etc/pki",
            // getpwuid/getgrgid sit in libc startup paths: without these many
            // programs cannot even resolve HOME.
            "/etc/passwd",
            "/etc/group",
            // Debian/Ubuntu toolchain symlinks (cc → gcc-13, etc.).
            "/etc/alternatives",
        ];
        #[cfg(target_os = "macos")]
        let listed = [
            "/bin",
            "/usr",
            // Rosetta's runtime, the loader analogue: no grant should name it.
            ROSETTA,
            "/Library/Developer/CommandLineTools",
            "/Applications/Xcode.app/Contents/Developer",
            "/lib",
            "/System",
            "/dev",
            "/private/var/db/dyld",
            // Wholesale: tools read whatever they read (gitconfig, paths.d,
            // zshenv, nix.conf, …); nothing user-secret sits here unprotected.
            "/private/etc",
            // xcode-select state; denied, build drivers report a broken install.
            "/private/var/select",
            // resolv.conf's target and the mDNSResponder socket.
            "/private/var/run",
        ];
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        let listed: [&str; 0] = [];
        let held = home.as_ref().and_then(|h| h.real.as_ref());
        let reads = resolve_reads(listed.map(std::path::PathBuf::from), held)?;
        let prefixes = toolchains::system_reads_in(&env, home.as_ref()).map_err(|e| e.message)?;
        let prefixes = prefixes
            .into_iter()
            .map(|dir| FsAllow::Prefix(dir.extent()));
        Ok(reads.into_iter().chain(prefixes).collect())
    }
}

/// Resolve system-read paths, dropping absent, foreign, non-Unicode and home paths.
pub(crate) fn resolve_reads(
    spellings: impl IntoIterator<Item = std::path::PathBuf>,
    held: Option<&RealPath>,
) -> Result<Vec<FsAllow>, String> {
    let mut reads = Vec::new();
    for spelling in spellings {
        let Some(written) = crate::path::is_absolute(&spelling)
            .then(|| FrozenPath::freeze(&spelling).ok())
            .flatten()
        else {
            continue;
        };
        let trace = walk_lenient(&written.lexical(), LeafMode::Follow).map_err(|e| {
            format!(
                "ral cannot resolve system-read path '{}': {e}",
                written.shown()
            )
        })?;
        if trace.kind().is_some() && toolchains::keeps_system_read(trace.real(), held) {
            reads.push(FsAllow::Prefix(
                GrantPath {
                    written,
                    real: trace,
                }
                .extent(),
            ));
        }
    }
    Ok(reads)
}
