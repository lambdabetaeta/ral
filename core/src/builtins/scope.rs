use crate::types::*;

use super::call_value;
use super::util::{as_map, sig};

pub(super) fn builtin_within(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.len() < 2 {
        return Err(sig("within requires 2 arguments (options_map, body)"));
    }
    let opts = as_map(&args[0], "within")?;
    let scope = WithinScope::parse(&opts, shell)?;
    let body = args[1].clone();
    crate::evaluator::audit::with_audited_scope(shell, "within", args, |shell| {
        scope.enter(shell, |shell| call_value(&body, &[], shell))
    })
}

/// The parsed `within [...]` options, ready to enter a scope.  Each key
/// becomes an `Shell::with_*` call, composed left-to-right in `enter`.
struct WithinScope {
    env_overrides: Option<std::collections::HashMap<String, String>>,
    cwd: Option<std::path::PathBuf>,
    handlers: Option<HandlerFrame>,
}

impl WithinScope {
    fn parse(opts: &[(String, Value)], shell: &mut Shell) -> Result<Self, EvalSignal> {
        let mut env_overrides = None;
        let mut cwd = None;
        let mut per_name: Vec<(String, Value)> = Vec::new();
        let mut catch_all: Option<Value> = None;
        let mut saw_handlers = false;

        for (k, v) in opts {
            match k.as_str() {
                "env" => env_overrides = Some(parse_env_overrides(v)?),
                "dir" => cwd = Some(parse_cwd(v, shell)?),
                "handlers" => {
                    per_name = parse_handler_map(v)?;
                    saw_handlers = true;
                }
                "handler" => {
                    catch_all = Some(parse_catch_all(v)?);
                    saw_handlers = true;
                }
                _ => return Err(sig(format!("within: unknown key '{k}'"))),
            }
        }

        let handlers = if saw_handlers {
            Some(HandlerFrame {
                per_name,
                catch_all,
            })
        } else {
            None
        };
        Ok(Self {
            env_overrides,
            cwd,
            handlers,
        })
    }

    /// Compose the parsed keys as nested `with_*` scopes around `body`.
    fn enter<R>(self, shell: &mut Shell, body: impl FnOnce(&mut Shell) -> R) -> R {
        let Self {
            env_overrides,
            cwd,
            handlers,
        } = self;
        let wrapped = |shell: &mut Shell| match handlers {
            Some(frame) => shell.with_handlers(frame, body),
            None => body(shell),
        };
        let wrapped = |shell: &mut Shell| match cwd {
            Some(path) => shell.with_cwd(path, wrapped),
            None => wrapped(shell),
        };
        match env_overrides {
            Some(o) => shell.with_env(o, wrapped),
            None => wrapped(shell),
        }
    }
}

fn parse_env_overrides(v: &Value) -> Result<std::collections::HashMap<String, String>, EvalSignal> {
    let overrides = as_map(v, "within env")?;
    overrides
        .into_iter()
        .map(|(ek, ev)| {
            let sv = match ev {
                Value::String(s) => s,
                Value::Int(n) => n.to_string(),
                Value::Float(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                other => {
                    return Err(sig(format!(
                        "within shell: value for '{ek}' must be a scalar (string, int, float, or bool), got {}",
                        other.type_name()
                    )));
                }
            };
            Ok((ek, sv))
        })
        .collect()
}

fn parse_cwd(v: &Value, shell: &mut Shell) -> Result<std::path::PathBuf, EvalSignal> {
    let path = v.to_string();
    if path.is_empty() {
        return Err(sig("within dir: path cannot be empty"));
    }
    shell.check_fs_read(&path)?;
    let resolved = shell.resolve_path(&path);
    if !resolved.is_dir() {
        return Err(sig(format!("within dir: {path}: not a directory")));
    }
    Ok(resolved)
}

fn parse_handler_map(v: &Value) -> Result<Vec<(String, Value)>, EvalSignal> {
    let map = as_map(v, "within handlers")?;
    map.into_iter()
        .map(|(cmd, thunk_val)| match thunk_val {
            Value::Thunk { .. } => Ok((cmd, thunk_val)),
            other => Err(sig(format!(
                "within handlers: value for '{cmd}' must be a block, got {}",
                other.type_name()
            ))),
        })
        .collect()
}

fn parse_catch_all(v: &Value) -> Result<Value, EvalSignal> {
    match v {
        Value::Thunk { .. } => Ok(v.clone()),
        other => Err(sig(format!(
            "within handler: must be a block, got {}",
            other.type_name()
        ))),
    }
}

pub(super) fn builtin_grant(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    use crate::types::{
        Capabilities, EditorCapability, ExecPolicy, FsPolicy, ShellCapability,
    };
    if args.len() < 2 {
        return Err(sig("grant requires 2 arguments (capabilities_map, body)"));
    }
    let caps = as_map(&args[0], "grant")?;

    // Each cap dimension stays `None` (no opinion → inherits caller) unless
    // the user explicitly named it.  This is *not* parse_capabilities's
    // deny-default behaviour: a plugin manifest declares a self-contained
    // ceiling, while a user grant attenuates *along the dimensions named*
    // and leaves the rest alone.  Critically this keeps `grant
    // [exec: [foo: []]] body` from triggering OS-level fs/net sandboxing
    // (no fs/net dimension is restricted, so no child sandbox is needed).
    let mut exec_policy: Option<Vec<(String, ExecPolicy)>> = None;
    let mut fs_policy: Option<FsPolicy> = None;
    let mut net_policy: Option<bool> = None;
    let mut audit_flag = false;
    let mut editor_policy: Option<EditorCapability> = None;
    let mut shell_policy: Option<ShellCapability> = None;

    for (k, v) in &caps {
        match k.as_str() {
            "exec" => {
                let exec_map = as_map(v, "grant exec")?;
                let mut entries = Vec::new();
                for (cmd, policy_val) in exec_map {
                    let policy = match policy_val {
                        Value::Bool(_) => {
                            return Err(sig(format!(
                                "grant exec: use [] to allow all subcommands for '{cmd}', not true/false"
                            )));
                        }
                        Value::List(items) => {
                            let subs: Vec<String> = items.iter().map(|i| i.to_string()).collect();
                            if subs.is_empty() {
                                ExecPolicy::Allow
                            } else {
                                ExecPolicy::Subcommands(subs)
                            }
                        }
                        Value::Thunk { .. } => {
                            return Err(sig(format!(
                                "grant exec: block form for '{cmd}' removed; use within [handlers: [{cmd}: ...]] instead"
                            )));
                        }
                        other => {
                            return Err(sig(format!(
                                "grant exec: policy for '{cmd}' must be a list of subcommands; got {}",
                                other.type_name()
                            )));
                        }
                    };
                    entries.push((cmd, policy));
                }
                exec_policy = Some(entries);
            }
            "fs" => {
                let fs_map = as_map(v, "grant fs")?;
                let mut fp = FsPolicy::default();
                for (sub, paths) in fs_map {
                    let list = match paths {
                        Value::List(items) => items.iter().map(|i| i.to_string()).collect(),
                        other => {
                            return Err(sig(format!(
                                "grant fs: '{sub}' must be a list of paths, got {} (use [\"/path\"])",
                                other.type_name()
                            )));
                        }
                    };
                    match sub.as_str() {
                        "read" => fp.read_prefixes = list,
                        "write" => fp.write_prefixes = list,
                        _ => return Err(sig(format!("grant fs: unknown key '{sub}'"))),
                    }
                }
                fs_policy = Some(fp);
            }
            "net" => {
                net_policy = Some(match v {
                    Value::Bool(b) => *b,
                    other => {
                        return Err(sig(format!(
                            "grant net: expected a Bool, got {}",
                            other.type_name()
                        )));
                    }
                });
            }
            "audit" => {
                audit_flag = matches!(v, Value::Bool(true));
            }
            "editor" => {
                let editor_map = as_map(v, "grant editor")?;
                let mut cap = EditorCapability {
                    read: false,
                    write: false,
                    tui: false,
                };
                for (field, fv) in editor_map {
                    match field.as_str() {
                        "read" => cap.read = matches!(fv, Value::Bool(true)),
                        "write" => cap.write = matches!(fv, Value::Bool(true)),
                        "tui" => cap.tui = matches!(fv, Value::Bool(true)),
                        _ => return Err(sig(format!("grant editor: unknown key '{field}'"))),
                    }
                }
                editor_policy = Some(cap);
            }
            "shell" => {
                let shell_map = as_map(v, "grant shell")?;
                let mut cap = ShellCapability::default();
                for (field, fv) in shell_map {
                    match field.as_str() {
                        "chdir" => cap.chdir = matches!(fv, Value::Bool(true)),
                        _ => return Err(sig(format!("grant shell: unknown key '{field}'"))),
                    }
                }
                shell_policy = Some(cap);
            }
            _ => return Err(sig(format!("grant: unknown key '{k}'"))),
        }
    }

    let ctx = Capabilities {
        exec: exec_policy,
        fs: fs_policy,
        net: net_policy,
        audit: audit_flag,
        editor: editor_policy,
        shell: shell_policy,
    };
    crate::evaluator::audit::with_audited_scope(shell, "grant", args, |shell| {
        shell.with_capabilities(ctx, |shell| crate::sandbox::eval_grant(&args[1], shell))
    })
}

#[cfg(test)]
mod tests {
    use super::builtin_grant;
    use crate::types::{Shell, Value};

    fn thunk() -> Value {
        let ast = crate::parse("{ }").unwrap();
        let comp = crate::elaborator::elaborate(&ast, Default::default());
        let mut shell = Shell::new(Default::default());
        crate::evaluate(&comp, &mut shell).unwrap()
    }

    #[test]
    fn grant_rejects_unknown_top_level_keys() {
        let mut shell = Shell::default();
        let body = thunk();
        let err = builtin_grant(
            &[Value::Map(vec![("fss".into(), Value::Map(vec![]))]), body],
            &mut shell,
        )
        .unwrap_err();

        match err {
            crate::types::EvalSignal::Error(e) => {
                assert!(e.message.contains("grant: unknown key 'fss'"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
