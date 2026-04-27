use ral_core::{Shell, EvalSignal, Value, diagnostic};

use super::errfmt::plugin_error;

use super::plugin::fold_hook;

/// USER, CWD, and STATUS values computed once per prompt, applied to both the
/// live shell and any child shell created for evaluating a thunk prompt.
pub(super) struct PromptBindings {
    user: String,
    cwd: String,
    status: i64,
}

impl PromptBindings {
    #[cfg(test)]
    pub(super) fn with(user: impl Into<String>, cwd: impl Into<String>, status: i64) -> Self {
        Self {
            user: user.into(),
            cwd: cwd.into(),
            status,
        }
    }

    fn collect(shell: &Shell) -> Self {
        let user = crate::platform::user_name();
        let cwd = std::env::current_dir().map_or_else(
            |_| "?".into(),
            |p| {
                let s = p.to_string_lossy().to_string();
                let home = crate::platform::home_dir();
                if !home.is_empty() && s.starts_with(&home) {
                    format!("~{}", &s[home.len()..])
                } else {
                    s
                }
            },
        );
        Self {
            user,
            cwd,
            status: i64::from(shell.control.last_status),
        }
    }

    /// Bind USER, CWD, STATUS in `shell`; the value namespace gets typed values,
    /// the ambient (process-shell) namespace gets stringified copies.
    fn apply(&self, shell: &mut Shell) {
        for (k, v, s) in self.entries() {
            shell.set(k.into(), v);
            shell.dynamic.env_vars.insert(k.into(), s);
        }
    }

    /// Same value-namespace bindings as [`apply`]; child envs do not own
    /// ambient state, so the stringified copies are dropped.
    fn apply_to_child(&self, child: &mut Shell) {
        for (k, v, _) in self.entries() {
            child.set(k.into(), v);
        }
    }

    fn entries(&self) -> [(&'static str, Value, String); 3] {
        [
            ("USER", Value::String(self.user.clone()), self.user.clone()),
            ("CWD", Value::String(self.cwd.clone()), self.cwd.clone()),
            ("STATUS", Value::Int(self.status), self.status.to_string()),
        ]
    }
}

pub(super) fn eval_prompt_block(prompt: &Value, shell: &Shell, bindings: &PromptBindings) -> String {
    let Value::Thunk { body, captured, .. } = prompt else {
        return "ral $ ".to_string();
    };

    let mut child = Shell::child_from(captured, shell);
    bindings.apply_to_child(&mut child);

    let (result, out) = ral_core::evaluator::with_capture(&mut child, |shell| {
        ral_core::evaluator::eval_block_body(body, shell)
    });

    match result {
        Ok(Value::Unit) => {
            let text = String::from_utf8_lossy(&out).into_owned();
            if let Some(stripped) = text.strip_suffix('\n') {
                stripped.to_string()
            } else {
                text
            }
        }
        Ok(other) => other.to_string(),
        Err(EvalSignal::Error(e)) => {
            diagnostic::cmd_error("ral", &format!("prompt error: {}", e.message));
            "ral $ ".to_string()
        }
        Err(_) => "ral $ ".to_string(),
    }
}

pub(super) fn build_prompt(shell: &mut Shell) -> String {
    if shell.io.terminal.ui_title_ok() {
        use std::io::Write;
        // CWD not yet computed; read current dir directly for the title.
        let cwd = std::env::current_dir()
            .map_or_else(|_| "?".into(), |p| p.to_string_lossy().to_string());
        let _ = std::io::stdout().write_all(format!("\x1b]0;ral: {cwd}\x07").as_bytes());
        let _ = std::io::stdout().flush();
    }

    let bindings = PromptBindings::collect(shell);
    bindings.apply(shell);

    let base = match shell.get("RAL_PROMPT").cloned() {
        Some(Value::String(p)) => p,
        Some(ref prompt @ Value::Thunk { .. }) => eval_prompt_block(prompt, shell, &bindings),
        _ => "ral $ ".to_string(),
    };

    fold_hook(shell, "prompt", base, |shell, name, handler, prompt| {
        match ral_core::evaluator::call_value_pub(handler, &[Value::String(prompt.clone())], shell) {
            Ok(Value::String(s)) => s,
            Err(EvalSignal::Error(e)) => {
                plugin_error(name, "hook 'prompt' failed", &e);
                prompt
            }
            _ => prompt,
        }
    })
}
