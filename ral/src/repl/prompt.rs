//! Prompt construction.
//!
//! The prompt body is a registered hook at `Session/"prompt"`, dispatched
//! like any other hook.  The prompt body reads `cwd` and `user` directly.
//! Plugins may transform the result via the `prompt` hook.

use ral_core::carrier::Transport;
use ral_core::errln;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum as _;
use ral_core::types::{Closure, DefaultPolicy, HookName, HookSig};
use ral_core::{Captured, Shell, Value};
use std::sync::Arc;

use super::host::{Outcome, ReplHost};
use super::plugin::{lock, manifest::HookEvent};

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
    let block = Value::Thunk(Closure::closed(ral_core::source::Spanned::synthetic(
        ral_core::ir::CompKind::Return(ral_core::ir::Val::String(DEFAULT_PROMPT.into())),
    )));
    let _ = shell.register_hook(name, block, HookSig::Block, DefaultPolicy::denied_capture());
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
pub(super) fn write_terminal_title(terminal: &ral_core::terminal::TerminalState, cwd: &str) {
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
/// over it (each read like the session prompt), and produce the renderable
/// [`PromptText`].  A failing hook falls back to [`DEFAULT_PROMPT`] (a plugin,
/// to the prompt so far); the render's faults print together, when they
/// differ from the last render's.
pub(super) fn render(t: &dyn Transport, host: &Arc<ReplHost>) -> PromptText {
    let base = host.run_hook(t, HookName::session("prompt"), vec![], None, None);
    let mut faults = Vec::new();
    let mut prompt = match base.outcome {
        Outcome::Settled(value) => prompt_text(value, base.captured),
        Outcome::Exited => DEFAULT_PROMPT.to_string(),
        Outcome::Faulted(fault) | Outcome::Walled(fault) => {
            faults.push(fault);
            DEFAULT_PROMPT.to_string()
        }
    };
    let plugins = lock(&host.runtime).with_hook(HookEvent::Prompt);
    for name in plugins {
        let hr = host.run_hook(
            t,
            HookName::plugin(name, <&str>::from(HookEvent::Prompt)),
            vec![prompt.clone().encode()],
            None,
            None,
        );
        match hr.outcome {
            Outcome::Settled(value) => prompt = prompt_text(value, hr.captured),
            Outcome::Exited => {}
            Outcome::Faulted(fault) | Outcome::Walled(fault) => faults.push(fault),
        }
    }
    if let Some(fault) = host.prompt_fault((!faults.is_empty()).then(|| faults.join("\n"))) {
        errln!("{fault}");
    }
    PromptText::from_styled(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            crate::PRELUDE.seat(shell);
            let prompt = crate::repl::eval(shell, &src);
            shell
                .register_hook(
                    HookName::session("prompt"),
                    prompt,
                    HookSig::Block,
                    DefaultPolicy::denied_capture(),
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

    // The ambient reads `cwd` and `user` are live; `USER` is pinned because
    // a container shell may run with none.

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "`!{…}` is ral splice syntax in the source under test"
    )]
    fn prompt_block_sees_ambient_reads() {
        let result =
            render_src("return { within [env: [USER: 'someone']] { return \"!{user}:!{cwd}\" } }");
        // Split at the first colon: a Windows `cwd` carries a drive
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
