//! Prompt construction.
//!
//! The prompt body is a registered hook at `Session/"prompt"`,
//! dispatched via [`Shell::run`].  CWD, STATUS, and USER are
//! ambient pseudo-variables read by the prompt body directly.
//! Plugins may transform the result via the `prompt` lifecycle hook.

use ral_core::transport::{Program, Run};
use ral_core::types::{Break, Capabilities, HookName};
use ral_core::{
    Captured, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Shell, Value,
    diagnostic,
};
#[cfg(test)]
use ral_core::{DefaultPolicy, HookSig};
use std::sync::{Arc, Mutex};

use super::plugin::{FramedHook, HookFraming, PluginRuntime, call_plugin_hook, fold_hook};

/// The default prompt.  Session boot registers it as the
/// `Session/"prompt"` hook (`install_default_prompt`); the failure
/// arms in [`render`] fall back to it directly, so a broken user thunk
/// degrades to the out-of-box prompt beside its per-render diagnostic.
/// The session survives a broken prompt: it is the place where the user
/// rebinds the prompt to fix it.
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

/// Build the capture-into-string run for the `Session/<name>` prompt hook.
/// The prompt body runs with denied terminal access and its stdout captured,
/// so a block that prints its prompt is read back from the capture.
fn prompt_run(name: &str) -> RunRequest<'static> {
    RunRequest {
        run: Run {
            program: Program::Hook {
                name: HookName::session(name),
                args: vec![],
            },
            script_name: "<prompt>".to_string(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Inherit,
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }
}

/// Extract the prompt display text from a prompt run's outcome.  A returned
/// value is the prompt; a returned unit falls back to captured stdout (with a
/// trailing newline trimmed); an error prints a diagnostic and degrades to
/// [`DEFAULT_PROMPT`].
fn prompt_text_from(result: Result<Value, Break>, captured: Option<Captured>) -> String {
    match result {
        Ok(Value::Unit) => {
            if let Some(cap) = captured {
                let text = String::from_utf8_lossy(&cap.stdout).into_owned();
                if let Some(stripped) = text.strip_suffix('\n') {
                    stripped.to_string()
                } else {
                    text
                }
            } else {
                DEFAULT_PROMPT.to_string()
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

/// Evaluate a prompt block, extracting its display text.  A block's return
/// value produces the prompt; when it returns unit, its captured stdout is
/// used.  Any other value is its display form, so a plain string prompt is
/// the string itself.  Registers the value as a temporary hook and runs it
/// through the same [`prompt_run`] / [`prompt_text_from`] path as [`render`].
#[cfg(test)]
pub(super) fn eval_prompt(prompt: &Value, shell: &mut Shell) -> String {
    let Value::Block { .. } = prompt else {
        return prompt.to_string();
    };

    let _ = shell.register_hook(
        HookName::session("__eval_prompt_test__"),
        prompt.clone(),
        HookSig::Prompt,
        DefaultPolicy::denied_capture(),
        ral_core::source::Span::synthetic(),
    );

    let (result, captured) =
        shell.with_preserved_status(
            |shell| match shell.run(prompt_run("__eval_prompt_test__")) {
                RunReport::Ran {
                    result, captured, ..
                } => (result, captured),
                RunReport::Static { .. } => {
                    unreachable!("a thunk prompt body never compiles source")
                }
            },
        );

    prompt_text_from(result, captured)
}

/// Write the terminal title escape (`ral: <cwd>`) to stdout.
///
/// Presentation-layer side effect, separate from the semantic prompt
/// computation in [`render`].  Called by the session loop before
/// rendering so the title updates whether or not the user changes the
/// prompt.  No-op on terminals that can't render OSC titles.
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

/// Run the registered `Session/"prompt"` hook, fold plugin `prompt`
/// hooks, and produce the renderable [`PromptText`].
///
/// The prompt hook is registered at session boot and may be
/// overwritten by the rc `prompt:` key.
pub(super) fn render(shell: &mut Shell, runtime: &Arc<Mutex<PluginRuntime>>) -> PromptText {
    let base = match shell.run(prompt_run("prompt")) {
        RunReport::Ran {
            result, captured, ..
        } => prompt_text_from(result, captured),
        RunReport::Static { .. } => DEFAULT_PROMPT.to_string(),
    };

    let final_prompt = fold_hook(
        runtime,
        shell,
        "prompt",
        base,
        |shell, plugin, hook, prompt| {
            // The prompt hook runs during `read`, outside any frame, and only
            // transforms the prompt string — it never foregrounds a child, so
            // it frames with `Denied`.
            let hr = call_plugin_hook(
                shell,
                plugin,
                hook,
                &[Value::String(prompt.clone())],
                None,
                HookFraming::Framed(FramedHook {
                    terminal: RequestedTerminalAccess::Denied,
                    kind: "prompt",
                    budget: None,
                }),
            );
            if let Ok(Value::String(s)) = hr.result {
                s
            } else {
                // No readline escape is pending at render time, so the
                // source-mapped fault (rendered while its registry was live)
                // prints immediately above the prompt.
                if let Some(rendered) = hr.rendered_error {
                    eprintln!("{rendered}");
                }
                prompt
            }
        },
    );

    PromptText::from_styled(final_prompt)
}

#[cfg(test)]
mod tests {
    use super::{PromptText, eval_prompt};
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
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        ral_core::builtins::register(&mut shell, crate::PRELUDE.comp());
        let ast = ral_core::syntax::parser::parse(src).unwrap();
        let comp = std::sync::Arc::new(ral_core::elaborator::elaborate(
            &ast,
            std::collections::HashSet::default(),
        ));
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
        assert_eq!(eval_prompt(&prompt, &mut shell), "ral $ ");
    }

    #[test]
    fn prompt_block_keeps_closure_captures_from_rc_scope() {
        let (mut shell, prompt) = evaluate_prompt_src(
            "let left = '['\n let right = ']'\n return { return \"$left ok $right\" }",
        );
        assert_eq!(eval_prompt(&prompt, &mut shell), "[ ok ]");
    }

    // ambient pseudo-variables ($CWD, $STATUS, $USER) are live.

    #[test]
    fn prompt_block_sees_pseudo_vars() {
        let source = "return { return \"$USER:$CWD:$STATUS\" }";
        let (mut shell, prompt) = evaluate_prompt_src(source);
        let result = eval_prompt(&prompt, &mut shell);
        let parts: Vec<&str> = result.split(':').collect();

        assert_eq!(parts.len(), 3, "expected user:cwd:status, got {result:?}");
        assert!(
            !parts[0].is_empty(),
            "USER must be non-empty, got {result:?}"
        );
        assert!(
            !parts[1].is_empty(),
            "CWD must be non-empty, got {result:?}"
        );
        assert_eq!(
            parts[2], "0",
            "STATUS must be 0 after successful eval, got {result:?}"
        );
    }

    #[test]
    fn string_prompt_renders_as_itself() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let prompt = Value::String("abc $ ".into());
        assert_eq!(eval_prompt(&prompt, &mut shell), "abc $ ");
    }

    #[test]
    fn failing_prompt_thunk_falls_back_to_default() {
        let (mut shell, prompt) = evaluate_prompt_src("{ fail [status: 1, message: 'boom'] }");
        assert_eq!(eval_prompt(&prompt, &mut shell), super::DEFAULT_PROMPT);
    }
}
