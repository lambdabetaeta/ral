use crate::types::*;
use std::path::Path;

use super::util::{as_list, check_arity, sig};

pub(super) fn builtin_path(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "_path")?;
    let op = args[0].to_string();
    let need_path = |label: &str| -> Result<String, EvalSignal> {
        match args.get(1) {
            Some(v) => Ok(v.to_string()),
            None => Err(sig(format!("_path '{label}' requires a path argument"))),
        }
    };
    match op.as_str() {
        "stem" | "ext" | "dir" | "base" => {
            let s = need_path(&op)?;
            let p = Path::new(&s);
            let r = match op.as_str() {
                "stem" => p.file_stem(),
                "ext" => p.extension(),
                "dir" => p.parent().map(|p| p.as_os_str()),
                "base" => p.file_name(),
                _ => unreachable!(),
            };
            Ok(Value::String(
                r.and_then(|s| s.to_str()).unwrap_or("").to_string(),
            ))
        }
        "resolve" => {
            let s = need_path("resolve")?;
            shell.check_fs_read(&s)?;
            let resolved =
                std::fs::canonicalize(&s).map_err(|e| sig(format!("resolve-path: {s}: {e}")))?;
            Ok(Value::String(resolved.to_string_lossy().into_owned()))
        }
        "join" => {
            check_arity(args, 2, "_path 'join'")?;
            let parts = as_list(&args[1], "path-join")?;
            let mut iter = parts.iter();
            let head = iter
                .next()
                .ok_or_else(|| sig("path-join: list must not be empty"))?;
            let mut path = std::path::PathBuf::from(head.to_string());
            for part in iter {
                path.push(part.to_string());
            }
            Ok(Value::String(path.to_string_lossy().into_owned()))
        }
        _ => Err(sig(format!("_path: unknown operation '{op}'"))),
    }
}

pub(super) fn builtin_which(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.len() != 1 {
        return Err(sig("which requires 1 argument"));
    }
    let name = args[0].to_string();
    if name.is_empty() {
        return Err(sig("which: command name cannot be empty"));
    }

    let line = which_line(&name, shell).ok_or_else(|| {
        EvalSignal::Error(Error::new(format!("which: {name}: not found"), 1))
    })?;
    shell.control.last_status = 0;
    Ok(Value::String(line))
}

/// Resolve `name` to a one-line description of where the shell would find it,
/// or `None` if it would fail.  Resolution order: local binding → prelude →
/// alias → builtin → handler / external on PATH.
fn which_line(name: &str, shell: &Shell) -> Option<String> {
    if shell.get_local(name).is_some() {
        return Some(format!("{name}: local"));
    }
    if shell.get_prelude(name).is_some() {
        return Some(format!("{name}: prelude"));
    }
    match shell.classify_command_head(name) {
        CommandHead::Alias => {
            let alias = shell.registry.aliases.get(name).unwrap();
            Some(format!("{name}: alias {}", format_alias(alias)))
        }
        CommandHead::Builtin => Some(format!("{name}: builtin")),
        CommandHead::GrantDenied => Some(format!("{name}: denied by grant")),
        CommandHead::External if shell.lookup_handler(name).is_some() => {
            Some(format!("{name}: handler"))
        }
        CommandHead::External => {
            find_external_command(name, shell).map(|p| p.to_string_lossy().into_owned())
        }
    }
}

fn format_alias(entry: &AliasEntry) -> String {
    match &entry.source {
        Some(src) => src.clone(),
        None => entry.value.to_string(),
    }
}

/// Resolve `p` against `shell.dynamic.cwd` if relative; otherwise return as-is.
fn anchor_to_cwd(p: std::path::PathBuf, shell: &Shell) -> std::path::PathBuf {
    if p.is_absolute() {
        return p;
    }
    match &shell.dynamic.cwd {
        Some(cwd) => cwd.join(p),
        None => p,
    }
}

fn find_external_command(name: &str, shell: &Shell) -> Option<std::path::PathBuf> {
    let has_sep =
        name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') || name.contains('\\');

    if has_sep {
        let candidate = anchor_to_cwd(std::path::PathBuf::from(name), shell);
        return command_candidate_exists(&candidate).then_some(candidate);
    }

    let path_value = shell
        .dynamic
        .env_vars
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))?;

    for dir in std::env::split_paths(&path_value) {
        let candidate = anchor_to_cwd(dir, shell).join(name);

        #[cfg(windows)]
        {
            for c in windows_command_candidates(&candidate) {
                if command_candidate_exists(&c) {
                    return Some(c);
                }
            }
        }
        #[cfg(not(windows))]
        {
            if command_candidate_exists(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_command_candidates(base: &std::path::Path) -> Vec<std::path::PathBuf> {
    use std::ffi::OsStr;

    let mut out = Vec::new();
    if base.extension().is_some() {
        out.push(base.to_path_buf());
    }

    let pathext = std::env::var_os("PATHEXT")
        .unwrap_or_else(|| OsStr::new(".COM;.EXE;.BAT;.CMD").to_os_string());

    for ext in pathext
        .to_string_lossy()
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        let ext = ext.trim_start_matches('.');
        out.push(base.with_extension(ext));
    }

    out
}

fn command_candidate_exists(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            return (meta.permissions().mode() & 0o111) != 0;
        }
        false
    }

    #[cfg(not(unix))]
    {
        true
    }
}
