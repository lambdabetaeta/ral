//! Prompt construction.
//!
//! Per-prompt bindings (USER, CWD, STATUS) are computed once and
//! installed on the live shell.  Plugins may transform the result via
//! the `prompt` lifecycle hook.

use ral_core::types::{Break, Capabilities, Env};
use ral_core::{RequestedTerminalAccess, Shell, TurnReport, Value, diagnostic};
use std::sync::{Arc, Mutex};

use super::plugin::{
    FramedHook, HookFraming, PluginRuntime, call_plugin_hook, fold_hook, framed_turn_request,
};

/// The default prompt.  Session boot templates it into the thunk it
/// binds to `RAL_PROMPT` (`install_default_prompt`); the failure arms in
/// [`eval_prompt`] fall back to it directly, so a broken user thunk
/// degrades to the out-of-box prompt beside its per-render diagnostic.
/// The session survives a broken prompt: it is the place where the user
/// rebinds `RAL_PROMPT` to fix it.
pub(super) const DEFAULT_PROMPT: &str = "❯ ";

/// Prompt text in both raw and styled forms.
///
/// `raw` is the visible prompt text with ANSI escape sequences stripped so
/// rustyline can compute cursor position correctly. `styled` preserves the
/// original prompt for terminals that can render it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PromptText {
    raw: String,
    styled: String,
}

impl PromptText {
    fn from_styled(styled: String) -> Self {
        let raw = ral_core::ansi::strip(&styled);
        Self { raw, styled }
    }

    pub(super) fn raw(&self) -> &str {
        &self.raw
    }

    pub(super) fn styled(&self) -> &str {
        &self.styled
    }
}

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
        let cwd = {
            let p = shell.cwd();
            let s = p.to_string_lossy().to_string();
            let home = crate::platform::home_dir();
            if !home.is_empty() && s.starts_with(&home) {
                format!("~{}", &s[home.len()..])
            } else if s.is_empty() {
                "?".into()
            } else {
                s
            }
        };
        Self {
            user,
            cwd,
            status: i64::from(shell.mobile.control.last_status),
        }
    }

    /// Bind USER, CWD, STATUS in `shell`; the value namespace gets typed values,
    /// the ambient (process-shell) namespace gets stringified copies.
    fn apply(&self, shell: &mut Shell) {
        for (k, v, s) in self.entries() {
            shell.mobile.scope.set(k.into(), v);
            shell.mobile.context.set_env_var(k, s);
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

/// Render a `RAL_PROMPT` value: a block is evaluated (its return value,
/// or its captured stdout when it returns unit); any other value is its
/// display form, so a plain string prompt is the string itself.
pub(super) fn eval_prompt(prompt: &Value, shell: &mut Shell, bindings: &PromptBindings) -> String {
    let Value::Block { body, captured } = prompt else {
        return prompt.to_string();
    };

    // USER / CWD / STATUS are dynamic per-call values; the closure's
    // lexical capture doesn't know about them.  Push a frame onto a
    // clone of `captured` and rebuild the thunk so the prompt body
    // resolves them through the value turn door, the same way every
    // other plugin hook in ral applies a thunk.
    let mut env: Env = (**captured).clone();
    env.push_scope();
    for (k, v, _) in bindings.entries() {
        env.set(k.into(), v);
    }
    let synthetic = Value::Block {
        body: body.clone(),
        captured: Arc::new(env),
    };

    // The prompt body's stdout is the prompt when it returns unit, so capture
    // it. `with_capture` carries the let-binding Seq semantics (non-final
    // stages flush to the visible stdout via `capture_outer`); `build_turn`
    // clones that capture context into the value turn's frame, so the body
    // runs under it. The prompt runs `Denied`: it must never foreground a
    // child. Save and restore `last_status` so the prompt body's own status
    // does not clobber the user's previous-command exit code visible at the
    // next prompt cycle (`PromptBindings::collect` reads it).
    let saved_status = shell.mobile.control.last_status;
    let (result, out) = ral_core::evaluator::with_capture(shell, |shell| {
        let req = framed_turn_request("<prompt>", RequestedTerminalAccess::Denied);
        match shell.run_value_turn(synthetic, vec![], "", req) {
            TurnReport::Ran { result, .. } => result,
            TurnReport::Static { .. } => unreachable!("a thunk prompt body never compiles source"),
        }
    });
    shell.mobile.control.last_status = saved_status;

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
        Err(Break::Error(e)) => {
            diagnostic::cmd_error("ral", &format!("prompt error: {}", e.message));
            DEFAULT_PROMPT.to_string()
        }
        Err(Break::Escape(_)) => DEFAULT_PROMPT.to_string(),
    }
}

/// Write the terminal title escape (`ral: <cwd>`) to stdout.
///
/// Presentation-layer side effect, separate from the semantic prompt
/// computation in [`render`].  Called by the session loop before
/// rendering so the title updates whether or not the user changes the
/// `RAL_PROMPT` value.  No-op on terminals that can't render OSC titles.
pub(super) fn write_terminal_title(shell: &Shell) {
    if !shell.terminal().ui_title_ok() {
        return;
    }
    use std::io::Write;
    let p = shell.cwd();
    let cwd = if p.as_os_str().is_empty() {
        "?".into()
    } else {
        p.to_string_lossy().into_owned()
    };
    let _ = std::io::stdout()
        .write_all(ral_core::ansi::osc_set_title(&format!("ral: {cwd}")).as_bytes());
    let _ = std::io::stdout().flush();
}

/// Collect the per-prompt bindings, install them, evaluate `RAL_PROMPT`,
/// fold plugin `prompt` hooks, and produce the renderable [`PromptText`].
///
/// `RAL_PROMPT` is always bound: session boot installs the default
/// thunk before rc sourcing, and value bindings can be overwritten but
/// never removed.
///
/// Side effects on `shell`: USER, CWD, STATUS land in both the value
/// namespace and the ambient env-var namespace (so child processes spawned
/// from inside the prompt see them too).
pub(super) fn render(shell: &mut Shell, runtime: &Arc<Mutex<PluginRuntime>>) -> PromptText {
    let bindings = PromptBindings::collect(shell);
    bindings.apply(shell);

    let prompt = shell
        .mobile
        .scope
        .get("RAL_PROMPT")
        .cloned()
        .expect("RAL_PROMPT is bound at session boot");
    let base = eval_prompt(&prompt, shell, &bindings);

    let final_prompt = fold_hook(
        runtime,
        shell,
        "prompt",
        base,
        |shell, plugin, handler, prompt| {
            // The prompt hook runs during `read`, outside any frame, and only
            // transforms the prompt string — it never foregrounds a child, so
            // it frames with `Denied`.
            let hr = call_plugin_hook(
                shell,
                plugin,
                handler,
                &[Value::String(prompt.clone())],
                None,
                HookFraming::Framed(FramedHook {
                    terminal: RequestedTerminalAccess::Denied,
                    kind: "prompt",
                    caps: Capabilities::root(),
                    budget: None,
                }),
            );
            match hr.result {
                Ok(Value::String(s)) => s,
                _ => {
                    // No readline escape is pending at render time, so the
                    // source-mapped fault (rendered while its registry was live)
                    // prints immediately above the prompt.
                    if let Some(rendered) = hr.rendered_error {
                        eprintln!("{rendered}");
                    }
                    prompt
                }
            }
        },
    );

    PromptText::from_styled(final_prompt)
}

#[cfg(test)]
mod tests {
    use super::{PromptBindings, PromptText, eval_prompt};
    use ral_core::{Shell, Value};

    #[test]
    fn strips_sgr_sequences_from_prompt_width() {
        let prompt = PromptText::from_styled("\x1b[31mred\x1b[0m $ ".to_string());
        assert_eq!(prompt.raw(), "red $ ");
        assert_eq!(prompt.styled(), "\x1b[31mred\x1b[0m $ ");
    }

    /// Parse and evaluate `src` to a thunk against a prelude-loaded shell.
    /// Returns `(shell, prompt_thunk)`.
    fn evaluate_prompt_src(src: &str) -> (Shell, Value) {
        let mut shell = Shell::new(Default::default());
        ral_core::builtins::register(&mut shell, crate::PRELUDE.comp());
        let ast = ral_core::syntax::parser::parse(src).unwrap();
        let comp = std::sync::Arc::new(ral_core::elaborator::elaborate(&ast, Default::default()));
        let prompt = ral_core::evaluator::evaluate(&comp, &mut shell).unwrap();
        assert!(
            matches!(prompt, Value::Lambda { .. } | Value::Block { .. }),
            "expected thunk"
        );
        (shell, prompt)
    }

    #[test]
    fn prompt_block_prefers_return_value_over_stdout() {
        let (mut shell, prompt) = evaluate_prompt_src("{ echo Darwin; return 'ral $ ' }");
        let bindings = PromptBindings::with("u", "/", 0);
        assert_eq!(eval_prompt(&prompt, &mut shell, &bindings), "ral $ ");
    }

    #[test]
    fn prompt_block_keeps_closure_captures_from_rc_scope() {
        let (mut shell, prompt) = evaluate_prompt_src(
            "let left = '['\n let right = ']'\n return { return \"$left ok $right\" }",
        );
        let bindings = PromptBindings::with("u", "/", 0);
        assert_eq!(eval_prompt(&prompt, &mut shell, &bindings), "[ ok ]");
    }

    #[test]
    fn prompt_block_sees_dynamic_prompt_bindings() {
        let (mut shell, prompt) = evaluate_prompt_src("return { return \"$USER:$CWD:$STATUS\" }");
        let bindings = PromptBindings::with("alice", "~/src", 7);
        assert_eq!(eval_prompt(&prompt, &mut shell, &bindings), "alice:~/src:7");
    }

    #[test]
    fn string_prompt_renders_as_itself() {
        let mut shell = Shell::new(Default::default());
        let bindings = PromptBindings::with("u", "/", 0);
        let prompt = Value::String("abc $ ".into());
        assert_eq!(eval_prompt(&prompt, &mut shell, &bindings), "abc $ ");
    }

    #[test]
    fn failing_prompt_thunk_falls_back_to_default() {
        let (mut shell, prompt) = evaluate_prompt_src("{ fail [status: 1, message: 'boom'] }");
        let bindings = PromptBindings::with("u", "/", 0);
        assert_eq!(
            eval_prompt(&prompt, &mut shell, &bindings),
            super::DEFAULT_PROMPT
        );
    }
}
