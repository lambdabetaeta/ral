//! Prompt construction.
//!
//! The prompt body is a registered hook at `Session/"prompt"`,
//! dispatched via [`Shell::run_hook`].  CWD, STATUS, and USER are
//! ambient pseudo-variables read by the prompt body directly.
//! Plugins may transform the result via the `prompt` lifecycle hook.

use ral_core::types::{Break, Capabilities, HookName};
#[cfg(test)]
use ral_core::{DefaultPolicy, HookSig};
use ral_core::{
    RequestedTerminalAccess, Shell, TurnIo, TurnReport, TurnRequest, TurnStdin, Value, diagnostic,
};
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

/// Evaluate a prompt block, extracting its display text.  A block's return
/// value produces the prompt; when it returns unit, its captured stdout is
/// used.  Any other value is its display form, so a plain string prompt is
/// the string itself.
#[cfg(test)]
pub(super) fn eval_prompt(prompt: &Value, shell: &mut Shell) -> String {
    let Value::Block { .. } = prompt else {
        return prompt.to_string();
    };

    // Register as a temporary hook, run it, extract text.
    let origin = ral_core::source::Span::new(ral_core::source::FileId(0), 0, 0);
    let _ = shell.register_hook(
        HookName::session("__eval_prompt_test__"),
        prompt.clone(),
        HookSig::Prompt,
        DefaultPolicy::denied_capture(),
        origin,
    );

    let req = TurnRequest {
        script_name: "<prompt>",
        caps: Capabilities::root(),
        io: TurnIo::Capture,
        terminal: RequestedTerminalAccess::Denied,
        stdin: TurnStdin::Inherit,
        turn_limit: None,
        detached_limit: None,
        surface: None,
        boundary: None,
        lifecycle: Box::new(()),
    };

    let (result, captured) = shell.with_preserved_status(|shell| {
        match shell.run_hook(&HookName::session("__eval_prompt_test__"), vec![], req) {
            TurnReport::Ran {
                result, captured, ..
            } => (result, captured),
            TurnReport::Static { .. } => {
                unreachable!("a thunk prompt body never compiles source")
            }
        }
    });

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
    let req = TurnRequest {
        script_name: "<prompt>",
        caps: Capabilities::root(),
        io: TurnIo::Capture,
        terminal: RequestedTerminalAccess::Denied,
        stdin: TurnStdin::Inherit,
        turn_limit: None,
        detached_limit: None,
        surface: None,
        boundary: None,
        lifecycle: Box::new(()),
    };

    let base = match shell.run_hook(&HookName::session("prompt"), vec![], req) {
        TurnReport::Ran {
            result, captured, ..
        } => match result {
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
        },
        TurnReport::Static { .. } => DEFAULT_PROMPT.to_string(),
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
        assert_eq!(eval_prompt(&prompt, &mut shell), "ral $ ");
    }

    #[test]
    fn prompt_block_keeps_closure_captures_from_rc_scope() {
        let (mut shell, prompt) = evaluate_prompt_src(
            "let left = '['\n let right = ']'\n return { return \"$left ok $right\" }",
        );
        assert_eq!(eval_prompt(&prompt, &mut shell), "[ ok ]");
    }

    // TODO: update for hook table — ambient $cwd/$user/$status need
    // per-test shell setup (user comes from platform::user_name())
    // #[test]
    // fn prompt_block_sees_dynamic_prompt_bindings() {
    //     let (mut shell, prompt) = evaluate_prompt_src("return { return \"$USER:$CWD:$STATUS\" }");
    //     let bindings = PromptBindings::with("alice", "~/src", 7);
    //     assert_eq!(eval_prompt(&prompt, &mut shell, &bindings), "alice:~/src:7");
    // }

    #[test]
    fn string_prompt_renders_as_itself() {
        let mut shell = Shell::new(Default::default());
        let prompt = Value::String("abc $ ".into());
        assert_eq!(eval_prompt(&prompt, &mut shell), "abc $ ");
    }

    #[test]
    fn failing_prompt_thunk_falls_back_to_default() {
        let (mut shell, prompt) = evaluate_prompt_src("{ fail [status: 1, message: 'boom'] }");
        assert_eq!(eval_prompt(&prompt, &mut shell), super::DEFAULT_PROMPT);
    }
}
