use crate::types::*;

use super::util::sig;

const MAX_SOURCE_DEPTH: usize = 100;

/// Save `shell.location.script` + `shell.location.call_site.script`, swap to `script` for
/// the guard's lifetime, and restore on drop.  `?`-returns from the body
/// roll the swap back automatically.  Shared between `source`, `use`, and
/// `eval_plugin_file`.
pub(super) struct ScriptContextGuard<'a> {
    shell: &'a mut Shell,
    saved_current_script: String,
    saved_call_site_script: String,
}

impl<'a> ScriptContextGuard<'a> {
    pub(super) fn enter(shell: &'a mut Shell, script: &str) -> Self {
        let saved_current_script = shell.location.script.clone();
        let saved_call_site_script = shell.location.call_site.script.clone();
        shell.location.script = script.to_string();
        shell.location.call_site.script = script.to_string();
        Self {
            shell,
            saved_current_script,
            saved_call_site_script,
        }
    }

    pub(super) fn env_mut(&mut self) -> &mut Shell {
        self.shell
    }
}

impl Drop for ScriptContextGuard<'_> {
    fn drop(&mut self) {
        self.shell.location.script = std::mem::take(&mut self.saved_current_script);
        self.shell.location.call_site.script = std::mem::take(&mut self.saved_call_site_script);
    }
}

pub(super) fn builtin_source(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let path = args.first().map(|v| v.to_string()).unwrap_or_default();
    let resolved = resolve_relative_to_current_script(&path, shell);
    let abs_path = std::fs::canonicalize(&resolved)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| resolved.to_string_lossy().into_owned());
    if shell.modules.stack.contains(&abs_path) {
        let cycle: Vec<&str> = shell.modules.stack.iter().map(|s| s.as_str()).collect();
        return Err(sig(format!(
            "source: circular dependency: {} -> {abs_path}",
            cycle.join(" -> ")
        )));
    }
    if shell.modules.depth >= MAX_SOURCE_DEPTH {
        return Err(sig(format!(
            "source: recursion depth limit ({MAX_SOURCE_DEPTH}) exceeded"
        )));
    }
    shell.check_fs_read(&abs_path)?;
    let source =
        std::fs::read_to_string(&resolved).map_err(|e| sig(format!("source: {path}: {e}")))?;
    let ast = crate::parse(&source).map_err(|e| sig(format!("source: {e}")))?;
    let mut ctx = ScriptContextGuard::enter(shell, &abs_path);
    ctx.env_mut().modules.stack.push(abs_path.clone());
    ctx.env_mut().modules.depth += 1;
    let result = {
        let c = crate::elaborator::elaborate(&ast, Default::default());
        crate::evaluate(&c, ctx.env_mut())
    };
    ctx.env_mut().modules.depth -= 1;
    ctx.env_mut().modules.stack.pop();
    result
}

pub(crate) fn builtin_use(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let path = args.first().map(|v| v.to_string()).unwrap_or_default();
    let resolved = resolve_relative_to_current_script(&path, shell);

    let abs_path = std::fs::canonicalize(&resolved)
        .or_else(|_| {
            if let Ok(ral_path) = std::env::var("RAL_PATH") {
                for dir in ral_path.split(':') {
                    let candidate = std::path::Path::new(dir).join(&path);
                    if let Ok(p) = std::fs::canonicalize(&candidate) {
                        return Ok(p);
                    }
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not found",
            ))
        })
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.clone());

    if let Some(cached) = shell.modules.cache.get(&abs_path) {
        return Ok(cached.clone());
    }

    if shell.modules.stack.contains(&abs_path) {
        let cycle: Vec<&str> = shell.modules.stack.iter().map(|s| s.as_str()).collect();
        return Err(sig(format!(
            "circular dependency: {} -> {abs_path}",
            cycle.join(" -> ")
        )));
    }

    shell.check_fs_read(&abs_path)?;
    let source = std::fs::read_to_string(&abs_path).map_err(|e| {
        sig(match e.kind() {
            std::io::ErrorKind::NotFound => format!("use: {path}: not found"),
            std::io::ErrorKind::PermissionDenied => format!("use: {path}: permission denied"),
            _ => format!("use: {path}: {e}"),
        })
    })?;
    let ast = crate::parse(&source).map_err(|e| sig(format!("use: {e}")))?;

    let mut ctx = ScriptContextGuard::enter(shell, &abs_path);
    ctx.env_mut().modules.stack.push(abs_path.clone());
    ctx.env_mut().push_scope();
    let eval_result = {
        let c = crate::elaborator::elaborate(&ast, Default::default());
        crate::evaluate(&c, ctx.env_mut())
    };

    let result = match eval_result {
        Ok(_) => {
            let bindings: Vec<(String, Value)> = ctx
                .env_mut()
                .top_scope()
                .iter()
                .filter(|(k, _)| !k.starts_with('_'))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let module = Value::Map(bindings);
            ctx.env_mut().modules.cache.insert(abs_path, module.clone());
            Ok(module)
        }
        Err(e) => Err(e),
    };

    ctx.env_mut().pop_scope();
    ctx.env_mut().modules.stack.pop();
    result
}

fn resolve_relative_to_current_script(path: &str, shell: &Shell) -> std::path::PathBuf {
    let input = std::path::PathBuf::from(path);
    if input.is_absolute() {
        return input;
    }
    let script = shell.location.script.as_str();
    if script.is_empty() || script.starts_with('<') {
        return input;
    }
    let base = std::path::Path::new(script)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    base.join(input)
}
