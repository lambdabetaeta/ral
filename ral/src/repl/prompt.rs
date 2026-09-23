//! Prompt construction.
//!
//! The prompt body is a registered hook at `Session/"prompt"`, dispatched
//! like any other hook.  CWD and USER are ambient pseudo-variables read by
//! the prompt body directly.  Plugins may transform the result via the
//! `prompt` lifecycle hook.

use ral_core::protocol::Transport;
use ral_core::serial::FOValue;
use ral_core::types::{Closure, DefaultPolicy, HookName, HookSig};
use ral_core::{Captured, Shell, Value};
use std::sync::Arc;

use super::host::ReplHost;
use super::plugin::lock;

/// The default prompt.  The boot door registers it as the `Session/"prompt"`
/// hook; a failing prompt falls back to it directly.  The session survives a
/// broken prompt: it is the place where the user rebinds the prompt to fix it.
pub(super) const DEFAULT_PROMPT: &str = "❯ ";

/// Register the `Session/"prompt"` hook returning [`DEFAULT_PROMPT`], unless
/// the rc registered one already.  Built as a thunk, not compiled from source,
/// so no boot-time `.expect` can panic on the constant's bytes.
pub(crate) fn install_default_prompt(shell: &mut Shell) {
    let name = HookName::session("prompt");
    if shell.has_hook(&name) {
        return;
    }
    let block = Value::Thunk(Closure {
        comp: Arc::new(ral_core::source::Spanned::synthetic(
            ral_core::ir::CompKind::Return(ral_core::ir::Val::String(DEFAULT_PROMPT.into())),
        )),
        env: ral_core::types::Env::default(),
    });
    let _ = shell.register_hook(
        name,
        block,
        HookSig::Prompt,
        DefaultPolicy::denied_capture(),
        ral_core::source::Span::synthetic(),
    );
}

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

/// A prompt run's text: a returned value is the prompt; a returned unit falls
/// back to its captured stdout, a trailing newline trimmed.
fn prompt_text(value: FOValue, captured: Option<Captured>) -> String {
    match (value, captured) {
        (FOValue::Unit, Some(cap)) => {
            let mut text = String::from_utf8_lossy(&cap.stdout).into_owned();
            if text.ends_with('\n') {
                text.pop();
            }
            text
        }
        (FOValue::Unit, None) => DEFAULT_PROMPT.to_string(),
        (value, _) => Value::from(value).to_string(),
    }
}

/// Write the terminal title escape (`ral: <cwd>`) to stdout.
///
/// Presentation-layer side effect, separate from the semantic prompt
/// computation in [`render`], so the title updates whether or not the user
/// changes the prompt.  No-op on terminals that can't render OSC titles.
pub(super) fn write_terminal_title(terminal: &ral_core::io::TerminalState, cwd: &str) {
    if !terminal.ui_title_ok() {
        return;
    }
    use std::io::Write;
    let cwd = if cwd.is_empty() { "?" } else { cwd };
    let _ = std::io::stdout()
        .write_all(ral_core::ansi::osc_set_title(&format!("ral: {cwd}")).as_bytes());
    let _ = std::io::stdout().flush();
}

/// Run the registered `Session/"prompt"` hook, fold plugin `prompt` hooks
/// over it, and produce the renderable [`PromptText`].  A failing prompt —
/// one whose value cannot cross included — falls back to [`DEFAULT_PROMPT`],
/// printing its diagnostic when it differs from the last one printed.
pub(super) fn render(t: &dyn Transport, host: &Arc<ReplHost>) -> PromptText {
    let base = host.run_hook(t, HookName::session("prompt"), vec![], None, None);
    if let Some(fault) = host.prompt_fault(base.fault) {
        eprintln!("{fault}");
    }
    let mut prompt = base.value.map_or_else(
        || DEFAULT_PROMPT.to_string(),
        |value| prompt_text(value, base.captured),
    );
    let plugins = lock(&host.runtime).with_hook("prompt", |_| true);
    for name in plugins {
        let hr = host.run_hook(
            t,
            HookName::plugin(name, "prompt"),
            vec![FOValue::String {
                value: prompt.clone(),
            }],
            None,
            None,
        );
        match (hr.value, hr.fault) {
            (Some(FOValue::String { value }), _) => prompt = value,
            (_, Some(fault)) => eprintln!("{fault}"),
            _ => {}
        }
    }
    PromptText::from_styled(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::source::Span;

    #[test]
    fn strips_sgr_sequences_from_prompt_width() {
        let prompt = PromptText::from_styled("\x1b[31mred\x1b[0m $ ".to_string());
        assert_eq!(prompt.raw(), "red $ ");
        assert_eq!(prompt.styled(), "\x1b[31mred\x1b[0m $ ");
    }

    /// Render the prompt a block evaluated from `src` gives, registered as
    /// the session prompt, through the transport.
    fn render_src(src: &str) -> String {
        let src = src.to_owned();
        let t = crate::repl::engine(move |shell| {
            ral_core::builtins::register(shell, crate::PRELUDE.comp());
            let prompt = crate::repl::eval(shell, &src);
            shell
                .register_hook(
                    HookName::session("prompt"),
                    prompt,
                    HookSig::Prompt,
                    DefaultPolicy::denied_capture(),
                    Span::synthetic(),
                )
                .expect("a block registers as the prompt");
        });
        render(&t, &ReplHost::new(Arc::default()))
            .styled()
            .to_string()
    }

    #[test]
    fn prompt_block_prefers_return_value_over_stdout() {
        assert_eq!(render_src("{ echo Darwin; return 'ral $ ' }"), "ral $ ");
    }

    #[test]
    fn prompt_block_keeps_closure_captures_from_rc_scope() {
        let src = "let left = '['\n let right = ']'\n return { return \"$left ok $right\" }";
        assert_eq!(render_src(src), "[ ok ]");
    }

    // ambient pseudo-variables ($CWD, $USER) are live.

    #[test]
    fn prompt_block_sees_pseudo_vars() {
        let result = render_src("return { return \"$USER:$CWD\" }");
        // Split at the first colon: a Windows `$CWD` carries a drive
        // colon of its own, and it lies to the right of this one.
        let (user, cwd) = result
            .split_once(':')
            .unwrap_or_else(|| panic!("expected user:cwd, got {result:?}"));
        assert!(!user.is_empty(), "USER must be non-empty, got {result:?}");
        assert!(!cwd.is_empty(), "CWD must be non-empty, got {result:?}");
    }

    #[test]
    fn failing_prompt_thunk_falls_back_to_default() {
        assert_eq!(
            render_src("{ fail [status: 1, message: 'boom'] }"),
            DEFAULT_PROMPT
        );
    }

    /// A prompt whose value cannot cross falls back rather than rendering it.
    #[test]
    fn a_prompt_returning_a_block_falls_back_to_default() {
        assert_eq!(render_src("{ return { echo x } }"), DEFAULT_PROMPT);
    }
}
