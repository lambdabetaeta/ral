//! `within` and `grant` — scoped overrides and capability attenuation.
//!
//! `within [...] body` composes `with_env`, `with_cwd`, and `with_handlers`
//! around a single call to `body`.  `grant [...] body` narrows the
//! ambient `Capabilities` along the dimensions named (`exec`, `fs`,
//! `net`, `editor`, `shell`, `audit`); unspecified dimensions inherit
//! the caller's frame.  Both run the body inside an audit scope so the
//! tree records the override.

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
    use crate::types::RawCapabilities;
    use super::caps;

    if args.len() < 2 {
        return Err(sig("grant requires 2 arguments (capabilities_map, body)"));
    }
    let caps_entries = as_map(&args[0], "grant")?;

    // Every dimension stays `None` (no opinion → inherits caller) unless
    // the user explicitly names it.  This is *not* the plugin-manifest
    // deny-default behaviour: a manifest declares a self-contained ceiling,
    // while a user grant attenuates *along the dimensions named* and leaves
    // the rest alone.  Critically this keeps `grant [exec: [foo: []]] body`
    // from triggering OS-level fs/net sandboxing (no fs/net dimension is
    // restricted, so no child sandbox is needed).
    let mut raw = RawCapabilities::default();
    for (k, v) in &caps_entries {
        match k.as_str() {
            "exec" => raw.exec = Some(caps::parse_exec_grant(v, "grant exec")?),
            "fs" => raw.fs = Some(caps::parse_fs(v, "grant fs", true, true)?),
            "net" => raw.net = Some(caps::parse_net(v, "grant net")?),
            "audit" => raw.audit = matches!(v, Value::Bool(true)),
            "editor" => raw.editor = Some(caps::parse_editor(v, "grant editor", true)?),
            "shell" => raw.shell = Some(caps::parse_shell(v, "grant shell", true)?),
            _ => return Err(sig(format!("grant: unknown key '{k}'"))),
        }
    }

    let home = shell.dynamic.home();
    let cwd = shell.dynamic.effective_cwd();
    let freeze_ctx = crate::path::sigil::FreezeCtx { home: &home, cwd: &cwd };
    let ctx = raw.freeze(&freeze_ctx).map_err(|e| sig(format!("grant: {e}")))?;
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
