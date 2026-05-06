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
            let resolved = crate::path::canon::canonicalise_strict(std::path::Path::new(&s))
                .map_err(|e| sig(format!("resolve-path: {s}: {e}")))?;
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
        // The classifier returns GrantDenied for any name not on the
        // allow side of the policy — including names that are nowhere
        // on the filesystem, since "no admission" is the same outcome
        // as "explicit deny" at the policy layer.  That's correct for
        // the gate but indistinguishable from "not installed" in a
        // `which` answer, so disambiguate here: only call it denied
        // when the binary is actually present.
        CommandHead::GrantDenied => shell
            .locate_command(name)
            .map(|p| format!("{name}: denied by grant ({})", p.display())),
        CommandHead::External if shell.lookup_handler(name).is_some() => {
            Some(format!("{name}: handler"))
        }
        CommandHead::External => shell
            .locate_command(name)
            .map(|p| p.to_string_lossy().into_owned()),
    }
}

fn format_alias(entry: &AliasEntry) -> String {
    match &entry.source {
        Some(src) => src.clone(),
        None => entry.value.to_string(),
    }
}
