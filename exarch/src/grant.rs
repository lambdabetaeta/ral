//! Grant policy emitted around every model command.
//!
//! `GrantSpec` is a typed view; `wrap` renders it as ral source.  The
//! prefix list contains both the lexical and canonicalised form of each
//! path so the match survives macOS's `/tmp` -> `/private/tmp` symlink.

use ral_core::types::{Capabilities, ExecPolicy, FsPolicy, ShellCapability};

/// Coreutils-only exec allowlist.  No shells, runtimes, or wildcards.
pub const CORE_EXEC: &[&str] = &[
    "arch", "b2sum", "base32", "base64", "basename", "basenc", "cat", "cksum", "comm", "cp",
    "csplit", "cut", "date", "df", "dir", "dircolors", "dirname", "du", "echo", "env", "expand",
    "expr", "factor", "fmt", "fold", "head", "hostname", "join", "link", "ln", "ls", "md5sum",
    "mkdir", "mktemp", "mv", "nl", "nproc", "numfmt", "od", "paste", "pr", "printenv", "printf",
    "ptx", "pwd", "readlink", "realpath", "rm", "rmdir", "seq", "sha1sum", "sha224sum",
    "sha256sum", "sha384sum", "sha512sum", "shred", "shuf", "sleep", "sort", "sum", "sync", "tac",
    "tail", "tee", "test", "touch", "tr", "truncate", "tsort", "uname", "unexpand", "uniq",
    "unlink", "vdir", "wc", "whoami", "yes",
];

/// Policy applied to the body of every tool call.
///
/// `Default` is the safe spec: the cwd + temp dir on FS, coreutils on
/// exec, net allowed.  `Dangerous` pushes ambient authority — every field on
/// `Capabilities` is `None` (no attenuation).  Pick `Dangerous` only
/// when something else is the trust boundary (e.g. a Docker container).
pub enum GrantSpec {
    Default { fs: FsPolicy, exec: Vec<String>, net: bool },
    Dangerous,
}

impl GrantSpec {
    /// Default: read/write the cwd and the platform temp dir(s); coreutils
    /// on exec; net allowed.  Both lexical and canonical forms go in.
    pub fn default_for(cwd: &str) -> Self {
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let mut prefixes = Vec::new();
        for p in [cwd, "/tmp", &tmp] {
            prefixes.push(p.to_string());
            prefixes.push(canon(p));
        }
        prefixes.sort();
        prefixes.dedup();
        Self::Default {
            fs: FsPolicy {
                read_prefixes: prefixes.clone(),
                write_prefixes: prefixes,
            },
            exec: CORE_EXEC.iter().map(|s| (*s).into()).collect(),
            net: true,
        }
    }

    /// Build a typed `Capabilities` value from this spec.  The exarch
    /// pushes this onto the shell's capability stack and calls
    /// `sandbox::eval_grant` directly, bypassing any source-level
    /// `grant { … }` syntax — there is no string-splice path the model
    /// could subvert with a stray `}`.
    pub fn to_capabilities(&self, audit: bool) -> Capabilities {
        match self {
            Self::Default { fs, exec, net } => Capabilities {
                exec: Some(
                    exec.iter()
                        .map(|n| (n.clone(), ExecPolicy::Allow))
                        .collect(),
                ),
                fs: Some(fs.clone()),
                net: Some(*net),
                audit,
                editor: None,
                shell: Some(ShellCapability { chdir: true }),
            },
            Self::Dangerous => Capabilities { audit, ..Capabilities::root() },
        }
    }
}

/// Canonicalise `p`; fall back to the input when resolution fails.
fn canon(p: &str) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.into())
}
