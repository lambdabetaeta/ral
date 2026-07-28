//! The loading half of exarch's capability composition: `ral_core`'s profile
//! loader dressed in exarch's error format, the confused-deputy lint, and the
//! cwd-relative path helper `super::for_invocation` calls around them.

use ral_core::types::{Break, Capabilities, Escape, Mooring, Shell};
use std::path::{Path, PathBuf};

use ral_core::path;
use ral_core::path::NormalizedPrefix;

/// Read a capabilities profile from `path` as a frozen [`Capabilities`].
///
/// Sigils freeze against `ctx` here, so a bad `xdg:` fails at the profile that
/// names it even where a later `meet` would have discarded the grant, and an
/// `exit` or stopped child inside the profile flattens into the error string:
/// a profile is configuration, not control flow.
pub(super) fn load_capabilities_ral(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    flag: &str,
    ctx: &ral_core::path::sigil::FreezeCtx<'_>,
) -> Result<Capabilities, String> {
    if !path.exists() {
        return Err(format!(
            "exarch: {flag} path does not exist: {}",
            path.display()
        ));
    }
    ral_core::capability::load_capabilities_from_path(mooring, shell, path, ctx).map_err(|e| {
        let detail = match e {
            Break::Error(err) => err.message,
            Break::Escape(Escape::Exit(code)) => format!("exit {code}"),
            #[cfg(unix)]
            Break::Escape(Escape::Stopped { signal, cmd, .. }) => {
                format!("{cmd}: stopped by signal {}", signal.display())
            }
        };
        format!("exarch: {flag} {}: {detail}", path.display())
    })
}

/// Warn — never deny — when the composed ceiling admits exec and write on one
/// prefix, as `ral_core::capability::deputy_prefixes` judges it.
///
/// Runs after every `join`/`meet` in [`for_invocation`](super::for_invocation):
/// two profiles can each be innocent and still meet into a deputy.
pub(super) fn lint_deputy_prefixes(caps: &Capabilities) {
    let found = ral_core::capability::deputy_prefixes(caps);
    if found.is_empty() {
        return;
    }
    let list = found
        .iter()
        .map(NormalizedPrefix::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "exarch: capability profile admits exec and write on the same prefix ({list}) — \
         a binary written there is admitted on the next call"
    );
}

/// Resolve `p` against `cwd` unless it is already absolute.
pub(super) fn absolute_in(cwd: &str, p: &Path) -> PathBuf {
    path::resolve_str(Some(cwd), &p.to_string_lossy())
}
