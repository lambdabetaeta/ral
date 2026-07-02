//! Prompt-box slash commands — registry, routing, and handlers.

use std::io;
use std::path::PathBuf;

use super::App;
use super::banner::{self, SessionInfo};
use super::block::RailShape;
use super::model_picker::pick_model;
use super::terminal::{YANK_CAP, osc52_copy, tail_bytes};
use super::tui_loop::Tui;
use super::viewport;
use crate::bus::{InboxMsg, Mailbox};
use crate::provider::scripted::Script;
use crate::provider::{Provider, ProviderKind};
use ral_core::path::sigil::expand_path_prefix;
pub(super) struct SlashCommand {
    pub(super) name: &'static str,
    pub(super) aliases: &'static [&'static str],
    /// The trailing argument the command consumes, e.g. `Some("<path>")`
    /// for `/export`.  `None` marks an argument-less command, which
    /// [`lookup_command`] matches only when typed alone — trailing text
    /// means the user meant a prompt, not the command.  Shown in `/help`.
    pub(super) arg: Option<&'static str>,
    pub(super) help: &'static str,
}

/// The slash-command registry — the single source of truth for the prompt-box
/// highlight ([`is_slash_command`]), the routing match ([`route_submit`]), and
/// the `/help` listing, so the three cannot drift.
pub(super) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        aliases: &[],
        arg: None,
        help: "List the available commands.",
    },
    SlashCommand {
        name: "/legend",
        aliases: &[],
        arg: None,
        help: "Decode the rail, bars, grain, and fidelity treatments.",
    },
    SlashCommand {
        name: "/clear",
        aliases: &[],
        arg: None,
        help: "Forget the conversation and clear the screen.",
    },
    SlashCommand {
        name: "/copy",
        aliases: &[],
        arg: None,
        help: "Copy the latest reply to the clipboard.",
    },
    SlashCommand {
        name: "/export",
        aliases: &[],
        arg: Some("<path>"),
        help: "Write the user view to a file.",
    },
    SlashCommand {
        name: "/model",
        aliases: &[],
        arg: None,
        help: "Switch the model or provider.",
    },
    SlashCommand {
        name: "/discuss",
        aliases: &[],
        arg: Some("<prompt>"),
        help: "Start a two-agent discussion and report back.",
    },
    SlashCommand {
        name: "/compact",
        aliases: &[],
        arg: None,
        help: "Summarize the conversation to reclaim context.",
    },
    SlashCommand {
        name: "/quit",
        aliases: &["/exit"],
        arg: None,
        help: "Leave exarch.",
    },
];

/// The command named by `trimmed`'s first token together with the trailing
/// argument it consumes, if any.  The first whitespace-delimited token is
/// matched against each command's name and aliases; the remainder, trimmed,
/// is the argument.  An argument-less command ([`SlashCommand::arg`] `None`)
/// matches only when typed alone — trailing text means the user meant a
/// prompt, so it declines and the line proceeds to the model.
pub(super) fn lookup_command(trimmed: &str) -> Option<(&'static SlashCommand, &str)> {
    let (head, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (trimmed, ""),
    };
    let cmd = SLASH_COMMANDS
        .iter()
        .find(|c| c.name == head || c.aliases.contains(&head))?;
    if cmd.arg.is_none() && !rest.is_empty() {
        return None;
    }
    Some((cmd, rest))
}

/// Whether `text`, as typed, is a recognized slash command — its first
/// token matched, mirroring [`lookup_command`]'s dispatch (so an
/// argument-less command with trailing text reads as a prompt, not a
/// command).
pub(super) fn is_slash_command(text: &str) -> bool {
    lookup_command(text.trim()).is_some()
}

/// Resolve a user-typed `/export` path: expand a leading `~`/`xdg:` sigil
/// against the home dir, then anchor a still-relative path at `cwd` (where
/// exarch was launched) so `/export notes.md` lands there rather than in
/// whatever directory the process happens to sit in.  [`resolve_str`] folds
/// `.`/`..` and joins the cwd.
pub(super) fn resolve_export_path(arg: &str, cwd: &str) -> PathBuf {
    let expanded = expand_path_prefix(arg, &ral_core::host::home());
    ral_core::path::resolve_str(Some(cwd), &expanded)
}

/// Emit one dim transcript line per registry entry: the command token
/// (with aliases) left-padded to a common width, then its description.
pub(super) fn cmd_help(app: &mut App) {
    let id = app.tabs.root();
    let names: Vec<String> = SLASH_COMMANDS
        .iter()
        .map(|c| {
            let mut s = c.name.to_string();
            if let Some(arg) = c.arg {
                s.push(' ');
                s.push_str(arg);
            }
            if !c.aliases.is_empty() {
                s.push_str(&format!(" ({})", c.aliases.join(", ")));
            }
            s
        })
        .collect();
    let width = names.iter().map(String::len).max().unwrap_or(0);
    for (n, c) in names.iter().zip(SLASH_COMMANDS) {
        app.push_note(id, format!("{n:<width$}   {}", c.help));
    }
}

/// Push the visual-vocabulary legend onto the transcript as ambient, rail-less
/// chrome — the panel that decodes the rail, bars, grain, and fidelity
/// treatments, rendered as the graphic's own samples.
pub(super) fn cmd_legend(app: &mut App) {
    app.push_chrome(app.tabs.root(), RailShape::Plain, banner::legend_panel());
}

/// Copy the latest assistant reply — the focused tab's trailing prose, as raw
/// markdown — to the system clipboard via OSC 52.  An oversized reply exceeds
/// the terminal's per-sequence limit, so copy its tail (bounded by `YANK_CAP`)
/// and say so, rather than let the terminal drop the whole sequence and copy
/// nothing silently.
pub(super) fn cmd_copy(app: &mut App) {
    let id = app.tabs.root();
    let reply = app.latest_reply();
    if reply.is_empty() {
        app.push_error(id, "no reply to copy yet".into());
        return;
    }
    let payload = tail_bytes(&reply, YANK_CAP);
    if let Err(e) = osc52_copy(payload) {
        app.push_error(id, format!("clipboard write failed: {e}"));
        return;
    }
    let note = if payload.len() < reply.len() {
        format!("[reply exceeds the clipboard limit — copied its last {YANK_CAP} bytes]")
    } else {
        format!(
            "[copied the latest reply — {} lines]",
            reply.lines().count()
        )
    };
    app.push_note(id, note);
}

/// Write the focused tab's user view — its rendered `user.log` — to `arg`, a
/// path that may be absolute, relative to the launch cwd, or `~`/`xdg:`-
/// prefixed.  Refuses to overwrite an existing file so an export never clobbers;
/// an empty argument prints the usage line.  The copy itself goes through
/// [`viewport::export_log`], where the `user.log` I/O door lives.
pub(super) fn cmd_export(app: &mut App, arg: &str, info: &SessionInfo<'_>) {
    let id = app.tabs.root();
    if arg.is_empty() {
        app.push_error(id, "usage: /export <path>".into());
        return;
    }
    let dest = resolve_export_path(arg, info.cwd);
    if dest.exists() {
        app.push_error(id, format!("refusing to overwrite {}", dest.display()));
        return;
    }
    let src = match app.flush_focused_log() {
        Ok(p) => p,
        Err(e) => {
            app.push_error(id, format!("could not flush transcript: {e}"));
            return;
        }
    };
    match viewport::export_log(&src, &dest) {
        Ok(_) => app.push_note(id, format!("[exported user view to {}]", dest.display())),
        Err(e) => app.push_error(id, format!("could not write {}: {e}", dest.display())),
    }
}

/// Route a submitted prompt line.  A view command (`/help`, `/legend`, `/copy`,
/// `/export`, `/model`) touches only the App, clipboard, file, or picker, so it
/// runs here on the UI thread.  A session command (`/clear`, `/compact`,
/// `/discuss`, `/quit`) and a plain prompt go onto the session inbox, where
/// the worker's drive loop drains them — `/clear` *also* clears the viewport
/// UI-side so the screen blanks immediately, before the worker rebuilds the
/// session.
pub(super) fn route_submit(
    text: String,
    tui: &mut Tui,
    mailbox: &Mailbox,
    ctx: &mut super::tui_loop::CommandCtx<'_>,
) -> io::Result<()> {
    let info = ctx.info;
    let trimmed = text.trim();
    match lookup_command(trimmed) {
        Some((cmd, arg)) => match cmd.name {
            "/help" => cmd_help(&mut tui.app),
            "/legend" => cmd_legend(&mut tui.app),
            "/copy" => cmd_copy(&mut tui.app),
            "/export" => cmd_export(&mut tui.app, arg, info),
            "/model" => {
                pick_model(tui, ctx)?;
            }
            // The viewport blanks immediately, and the in-flight model response
            // is cancelled first — otherwise streamed tokens sitting in the bus
            // keep flowing into the cleared viewport until the worker, parked
            // inside `apply`, hits its next poll (50 ms) and the model's turn
            // ends on its own.  Raising the interrupt cancels the trunk's
            // published token and the ral foreground, exactly as Esc does; the
            // subtree cascade reaps any live descendants now rather than after
            // the worker reaches the `Turn::Command`.  Stragglers already in the
            // unbounded bus channel are dropped in `App::handle` by the
            // clear-drain guard `root_clear_drain` arms.  Then the `/clear`
            // itself reaches the worker's drive loop and rebuilds the session.
            "/clear" => {
                let root = tui.app.tabs.root();
                crate::cancel::raise_interrupt();
                ctx.agents.cancel(root);
                let focused = tui.app.tabs.focused();
                // Read the focused agent's provider for the banner redraw.
                // If the focused agent has settled (no provider), fall back to
                // the root's provider.  If neither is available, use a
                // throwaway scripted provider.
                let provider_guard = ctx
                    .agents
                    .provider(focused)
                    .map(|ph| ph.current())
                    .or_else(|| ctx.agents.provider(root).map(|ph| ph.current()));
                if let Some(guard) = provider_guard {
                    tui.app.clear(info, &guard, tui.guard.term())?;
                } else {
                    let fallback =
                        Provider::scripted("unknown", ProviderKind::Openai, Script::new());
                    tui.app.clear(info, &fallback, tui.guard.term())?;
                }
                mailbox.push(InboxMsg::Command("/clear".into()));
            }
            "/discuss" => {
                if arg.is_empty() {
                    tui.app
                        .push_error(tui.app.tabs.root(), "usage: /discuss <prompt>".into());
                    return Ok(());
                }
                mailbox.push(InboxMsg::Command(text.clone()));
            }
            // The worker's `ReplControl` compacts the history / returns Quit.
            _ => mailbox.push(InboxMsg::Command(text.clone())),
        },
        // A plain prompt: onto the session inbox for the worker to drain.
        None => mailbox.push_user(text),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{lookup_command, resolve_export_path};

    /// The matched command's canonical name plus the argument
    /// `lookup_command` peeled off — `None` when nothing matched.
    fn dispatch(input: &str) -> Option<(&'static str, String)> {
        lookup_command(input).map(|(c, arg)| (c.name, arg.to_string()))
    }

    #[test]
    fn argless_command_matches_alone_but_not_with_trailing_text() {
        assert_eq!(dispatch("/copy"), Some(("/copy", String::new())));
        // Trailing text on an argument-less command is not that command: it
        // falls through to the model as a prompt rather than running /copy.
        assert_eq!(dispatch("/copy this"), None);
        // An alias resolves to its canonical entry.
        assert_eq!(dispatch("/exit"), Some(("/quit", String::new())));
    }

    #[test]
    fn export_consumes_its_path_argument() {
        assert_eq!(
            dispatch("/export ~/notes.md"),
            Some(("/export", "~/notes.md".to_string()))
        );
        // Whitespace around the argument is trimmed.
        assert_eq!(
            dispatch("/export   /tmp/a.txt  "),
            Some(("/export", "/tmp/a.txt".to_string()))
        );
        // A bare /export still matches, with the empty argument its handler
        // turns into the usage hint.
        assert_eq!(dispatch("/export"), Some(("/export", String::new())));
    }

    #[test]
    fn discuss_consumes_its_prompt_argument() {
        assert_eq!(
            dispatch("/discuss should we add a new channel?"),
            Some(("/discuss", "should we add a new channel?".to_string()))
        );
        assert_eq!(dispatch("/discuss"), Some(("/discuss", String::new())));
    }

    #[test]
    fn unknown_token_is_not_a_command() {
        assert_eq!(dispatch("/bogus"), None);
        assert_eq!(dispatch("just a prompt"), None);
    }

    #[test]
    fn export_path_resolves_absolute_and_relative() {
        // An absolute path passes through (dots folded, cwd ignored).
        assert_eq!(
            resolve_export_path("/tmp/out.txt", "/Users/me/proj").to_str(),
            Some("/tmp/out.txt")
        );
        // A relative path anchors at the launch cwd, not the process cwd.
        assert_eq!(
            resolve_export_path("notes.md", "/Users/me/proj").to_str(),
            Some("/Users/me/proj/notes.md")
        );
    }
}
