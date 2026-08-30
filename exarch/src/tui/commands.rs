//! Prompt-box slash commands — registry, routing, and handlers.

use std::fmt::Write;
use std::io;
use std::path::PathBuf;

use super::App;
use super::banner::{self, SessionInfo};
use super::block::{ChromeKind, Reveal};
use super::login;
use super::model_picker::pick_model;
use super::terminal::{YANK_CAP, osc52_copy, tail_bytes};
use super::tui_loop::Tui;
use super::viewport;
use crate::bus::{Mailbox, Post};
use prompt_editor::completion::Candidate;
use ral_core::path::sigil::expand_path_prefix;
pub(super) struct SlashCommand {
    pub(super) name: &'static str,
    pub(super) aliases: &'static [&'static str],
    /// The trailing argument, e.g. `Some("<path>")` for `/export`; `None` marks
    /// a command that matches only when typed alone.
    pub(super) arg: Option<&'static str>,
    /// Whether the command runs wherever it is typed.  A command that reaches
    /// the session inbox belongs to the trunk's context and is refused off
    /// it; one that touches only the view runs on any tab.  Declared here
    /// rather than hand-listed in [`route_submit`], so a command cannot be
    /// added without saying which it is.
    pub(super) any_tab: bool,
    /// Whether the command rewrites or ends the session, as against merely
    /// reading it.  A rewrite rides the inbox as a [`Post::Barrier`] and holds
    /// every prompt queued behind it at the exchange boundary, since it changes
    /// what those prompts would mean; a read (`/branch` forks a projection of
    /// the context, `/context` and `/resources` survey it) lets them pass and
    /// reach the model mid-exchange.  Read only of a command that reaches the
    /// inbox at all — `any_tab` above is what says which those are — but
    /// declared for every one, here rather than hand-listed in [`route_submit`],
    /// so a command cannot be added without saying which it is.
    pub(super) rewrites: bool,
    pub(super) help: &'static str,
}

/// The one registry behind the prompt-box highlight, the routing match, and the
/// `/help` listing, so the three cannot drift.
pub(super) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        aliases: &[],
        arg: None,
        rewrites: false,
        any_tab: false,
        help: "List the available commands.",
    },
    SlashCommand {
        name: "/legend",
        aliases: &[],
        arg: None,
        rewrites: false,
        any_tab: false,
        help: "Decode the rail, bars, grain, and fidelity treatments.",
    },
    SlashCommand {
        name: "/thinking",
        aliases: &[],
        arg: None,
        rewrites: false,
        any_tab: true,
        help: "Collapse or expand every thinking trace, on screen and to come.",
    },
    SlashCommand {
        name: "/clear",
        aliases: &[],
        arg: None,
        rewrites: true,
        any_tab: false,
        help: "Forget the conversation and clear the screen.",
    },
    SlashCommand {
        name: "/copy",
        aliases: &[],
        arg: None,
        rewrites: false,
        any_tab: false,
        help: "Copy the latest reply to the clipboard.",
    },
    SlashCommand {
        name: "/export",
        aliases: &[],
        arg: Some("<path>"),
        rewrites: false,
        any_tab: false,
        help: "Write the user view to a file.",
    },
    SlashCommand {
        name: "/model",
        aliases: &[],
        arg: None,
        rewrites: false,
        any_tab: false,
        help: "Switch the model or provider.",
    },
    SlashCommand {
        name: "/login",
        aliases: &[],
        arg: None,
        rewrites: false,
        any_tab: false,
        help: "Sign in with ChatGPT — adds a plan-backed provider.",
    },
    SlashCommand {
        name: "/branch",
        aliases: &[],
        arg: Some("[name]"),
        rewrites: false,
        any_tab: false,
        help: "Fork this conversation into a new tab (same context), under a name you choose.",
    },
    SlashCommand {
        name: "/close",
        aliases: &[],
        arg: None,
        rewrites: false,
        any_tab: true,
        help: "Close this branch (its tab and any agents it spawned).",
    },
    SlashCommand {
        name: "/focus",
        aliases: &[],
        arg: Some("<name>"),
        rewrites: false,
        any_tab: true,
        help: "Jump focus to a tab by name — reaches one TAB skips (demoted).",
    },
    SlashCommand {
        name: "/compact",
        aliases: &[],
        arg: None,
        rewrites: true,
        any_tab: false,
        help: "Summarize the conversation to reclaim context.",
    },
    SlashCommand {
        name: "/context",
        aliases: &[],
        arg: None,
        rewrites: false,
        any_tab: false,
        help: "Survey the model context without changing it.",
    },
    SlashCommand {
        name: "/rewind",
        aliases: &[],
        arg: Some("<exchange>"),
        rewrites: true,
        any_tab: false,
        help: "Drop context from an exchange; descendants and the shell are untouched.",
    },
    SlashCommand {
        name: "/resources",
        aliases: &[],
        arg: None,
        rewrites: false,
        any_tab: false,
        help: "Show the agent's resource probes: workers, inbox, log, disk.",
    },
    SlashCommand {
        name: "/quit",
        aliases: &["/exit"],
        arg: None,
        rewrites: true,
        any_tab: false,
        help: "Leave exarch.",
    },
];

/// The command a token names, by its own name or by one of its aliases.
fn by_token(token: &str) -> Option<&'static SlashCommand> {
    SLASH_COMMANDS
        .iter()
        .find(|c| c.name == token || c.aliases.contains(&token))
}

/// The command named by `trimmed`'s first token, with the trimmed remainder as
/// its argument.  An argument-less command matches only when typed alone, so
/// `/copy this` declines and the line proceeds to the model as a prompt.
pub(super) fn lookup_command(trimmed: &str) -> Option<(&'static SlashCommand, &str)> {
    let (head, rest) = split_head(trimmed);
    let cmd = by_token(head)?;
    if cmd.arg.is_none() && !rest.is_empty() {
        return None;
    }
    Some((cmd, rest))
}

/// Whether `line` is still a bare command token: a `/` and the characters a
/// command name is spelled with.  A space ends the token, and with it the
/// popup — what follows is an argument, which the registry cannot complete.
fn composing_command(line: &str) -> bool {
    line.strip_prefix('/').is_some_and(|rest| {
        rest.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

/// What `line` could still become, best first: every command name and every
/// alias that fuzzy-matches it, the bare `/` matching all of them.
///
/// An alias stands for itself — accepting one from `/ex` yields `/exit`, not
/// the `/quit` it routes to — and the trailing argument rides the display
/// alone, so the user types the space that separates it from the command.
/// The registry's `help` line rides the detail column, likewise never spliced.
pub(super) fn command_candidates(line: &str) -> Vec<Candidate> {
    if !composing_command(line) {
        return Vec::new();
    }
    let tokens: Vec<&'static str> = SLASH_COMMANDS
        .iter()
        .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied()))
        .collect();
    ral_core::text::rank(line, tokens, false)
        .into_iter()
        .map(|token| {
            let cmd = by_token(token);
            Candidate {
                display: match cmd.and_then(|c| c.arg) {
                    Some(arg) => format!("{token} {arg}"),
                    None => token.to_string(),
                },
                detail: cmd.map(|c| c.help.to_string()),
                replacement: token.to_string(),
            }
        })
        .collect()
}

/// Whether `text` names a command — the prompt-box highlight, run through
/// [`lookup_command`] so the highlight and the dispatch cannot disagree.
pub(super) fn is_slash_command(text: &str) -> bool {
    lookup_command(text.trim()).is_some()
}

/// Split off the first whitespace-delimited token — the head/rest shape both
/// [`lookup_command`] and the worker's `ReplControl::command` parse into.
pub(super) fn split_head(trimmed: &str) -> (&str, &str) {
    match trimmed.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (trimmed, ""),
    }
}

/// The head token when it starts with `/` and names no command — a typo like
/// `/bogus`, unlike `/copy this`, whose trailing text makes it a deliberate
/// fall-through to the model.
pub(super) fn unrecognized_command(trimmed: &str) -> Option<&str> {
    let head = trimmed.split_whitespace().next()?;
    (head.starts_with('/') && by_token(head).is_none()).then_some(head)
}

/// Resolve a typed `/export` path: expand a `~`/`xdg:` head, then anchor a
/// still-relative path at the launch `cwd` rather than the process's own.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: a path the operator types at the TUI means the operator's own `~`"
)]
pub(super) fn resolve_export_path(arg: &str, cwd: &str) -> PathBuf {
    let expanded = expand_path_prefix(arg, ral_core::host::home().as_deref());
    ral_core::path::resolve_str(Some(cwd), &expanded)
}

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
                let _ = write!(s, " ({})", c.aliases.join(", "));
            }
            s
        })
        .collect();
    let width = names.iter().map(String::len).max().unwrap_or(0);
    for (n, c) in names.iter().zip(SLASH_COMMANDS) {
        app.push_note(id, &format!("{n:<width$}   {}", c.help));
    }
}

pub(super) fn cmd_legend(app: &mut App) {
    app.push_chrome(app.tabs.root(), ChromeKind::Plain, banner::legend_panel());
}

/// Flip the disclosure of thinking traces everywhere at once: one setting, so a
/// trace already on screen and one that arrives an hour from now read alike.
pub(super) fn cmd_thinking(app: &mut App) {
    let id = app.tabs.focused();
    let note = match app.tabs.toggle_traces() {
        Reveal::Full => "[thinking traces expanded]",
        _ => "[thinking traces collapsed to their headers]",
    };
    app.push_note(id, note);
}

/// Copy the latest reply, as raw markdown, to the clipboard via OSC 52.  A reply
/// past the terminal's per-sequence limit is copied tail-first and announced,
/// since the terminal would otherwise drop the sequence and copy nothing.
pub(super) fn cmd_copy(app: &mut App) {
    let id = app.tabs.root();
    let reply = app.latest_reply();
    if reply.is_empty() {
        app.push_error(id, "no reply to copy yet");
        return;
    }
    let payload = tail_bytes(&reply, YANK_CAP);
    if let Err(e) = osc52_copy(payload) {
        app.push_error(id, &format!("clipboard write failed: {e}"));
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
    app.push_note(id, &note);
}

/// Write the focused tab's rendered `user.log` to `arg`, never over an existing
/// file.  The copy goes through [`viewport::export_log`], the I/O door.
pub(super) fn cmd_export(app: &mut App, arg: &str, info: &SessionInfo<'_>) {
    let id = app.tabs.root();
    if arg.is_empty() {
        app.push_error(id, "usage: /export <path>");
        return;
    }
    let dest = resolve_export_path(arg, info.cwd);
    if dest.exists() {
        app.push_error(id, &format!("refusing to overwrite {}", dest.display()));
        return;
    }
    let src = match app.flush_focused_log() {
        Ok(p) => p,
        Err(e) => {
            app.push_error(id, &format!("could not flush transcript: {e}"));
            return;
        }
    };
    match viewport::export_log(&src, &dest) {
        Ok(_) => app.push_note(id, &format!("[exported user view to {}]", dest.display())),
        Err(e) => app.push_error(id, &format!("could not write {}: {e}", dest.display())),
    }
}

/// Jump focus to the live tab named `arg` — the only way onto a demoted tab,
/// which `TAB` skips.  The name resolves, but nothing is renewed: attention
/// alone must not keep a child alive.
pub(super) fn cmd_focus(app: &mut App, arg: &str) {
    let id = app.tabs.focused();
    if arg.is_empty() {
        app.push_error(id, "usage: /focus <name>");
        return;
    }
    match app.tabs.by_name(arg) {
        Some(target) => app.tabs.set_focus(target),
        None => app.push_error(id, &format!("no live tab named {arg}")),
    }
}

impl SlashCommand {
    /// The typed `line` as an inbox post, under the boundary this command
    /// declares.
    fn post(&self, line: String) -> Post {
        if self.rewrites {
            Post::Barrier(line)
        } else {
            Post::Command(line)
        }
    }
}

/// The one submit path for every tab: parse once, then act on the parse and the
/// focused tab.  A view command (`/help`, `/legend`, `/copy`, `/export`,
/// `/model`, `/login`, `/thinking`) touches only the App, clipboard, file, or picker, so it
/// runs here on the UI thread; the rest ride the session inbox to the worker's
/// `ReplControl`, which owns the trunk's context.  A command typed on a sub-agent
/// tab is therefore refused rather than misfired — a sub-agent attends under
/// `NoControl`, and the trunk's inbox would act on the wrong session — save
/// those the registry marks `any_tab`, which touch no inbox.  A plain line
/// steers the focused tab instead.  Errors land on the focused tab, where the
/// user typed.
pub(super) fn route_submit(
    text: String,
    tui: &mut Tui,
    mailbox: &Mailbox,
    ctx: &mut super::tui_loop::CommandCtx<'_>,
) -> io::Result<()> {
    let info = ctx.info;
    let trimmed = text.trim();
    let root = tui.app.tabs.root();
    let focused = tui.app.tabs.focused();
    let unrecognized = unrecognized_command(trimmed);
    match lookup_command(trimmed) {
        Some((cmd, _)) if focused != root && !cmd.any_tab => {
            tui.app.push_error(
                focused,
                &format!("{} is not available on this tab", cmd.name),
            );
        }
        Some((cmd, arg)) => match cmd.name {
            "/close" => {
                if focused == root {
                    tui.app
                        .push_error(root, "nothing to close here; /quit ends the session");
                } else if !tui.app.tabs.is_branch(focused) {
                    tui.app
                        .push_error(focused, "/close closes a branch, not this tab");
                } else if let Some(agent) = tui.app.tabs.focused_agent() {
                    agent.cancel_tree(ral_core::process::CancelCause::Explicit);
                } else {
                    tui.app.push_error(
                        focused,
                        "this branch has already ended; its tab fades on its own",
                    );
                }
            }
            "/focus" => cmd_focus(&mut tui.app, arg),
            "/thinking" => cmd_thinking(&mut tui.app),
            "/help" => cmd_help(&mut tui.app),
            "/legend" => cmd_legend(&mut tui.app),
            "/copy" => cmd_copy(&mut tui.app),
            "/export" => cmd_export(&mut tui.app, arg, info),
            "/model" => {
                pick_model(tui, ctx);
            }
            "/login" => login::login(tui, ctx),
            // Cancel before blanking: tokens already in flight would otherwise
            // paint into the cleared viewport until the worker's next poll, and
            // what the bus still holds `App::handle`'s clear-drain drops.
            // Descendants only — a terminate-class cause on the trunk's own
            // token is permanent, and `/clear` rebuilds the trunk in place.
            // The pre-blank cancel wants the trunk's in-flight dispatch by
            // handle too, not only the ambient stamp.
            "/clear" => {
                crate::agent::cancel::raise_interrupt();
                if let Some(agent) = tui.app.tabs.agent(root) {
                    agent.interrupt();
                    agent.cancel_descendants(ral_core::process::CancelCause::Explicit);
                }
                tui.app.clear(info, tui.guard.term())?;
                mailbox.push(cmd.post("/clear".into()));
            }
            _ => mailbox.push(cmd.post(text.clone())),
        },
        // A typo is not a prompt in disguise: say so rather than mail it to the
        // model as one.
        None if unrecognized.is_some() => {
            let head = unrecognized.expect("checked Some above");
            tui.app
                .push_error(focused, &format!("unknown command: {head} (see /help)"));
        }
        // `steer` is the one delivery door: it renews the agent's idle lease,
        // and the line is dropped if that agent died since it was focused.
        None => {
            if focused == root {
                // A model-less launch would reach the provider with an empty
                // model and fail on the wire; point at `/model` instead.
                if tui
                    .app
                    .tabs
                    .agent(root)
                    .is_some_and(|agent| agent.current_provider().model().is_empty())
                {
                    tui.app
                        .push_error(root, "no model selected — run /model to choose one");
                    return Ok(());
                }
                mailbox.push_user(text);
            } else if let Some(agent) = tui.app.tabs.agent(focused) {
                agent.mailbox().steer(text);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        SLASH_COMMANDS, command_candidates, lookup_command, resolve_export_path,
        unrecognized_command,
    };

    fn replacements(line: &str) -> Vec<String> {
        command_candidates(line)
            .into_iter()
            .map(|c| c.replacement)
            .collect()
    }

    fn dispatch(input: &str) -> Option<(&'static str, String)> {
        lookup_command(input).map(|(c, arg)| (c.name, arg.to_string()))
    }

    #[test]
    fn argless_command_matches_alone_but_not_with_trailing_text() {
        assert_eq!(dispatch("/copy"), Some(("/copy", String::new())));
        assert_eq!(dispatch("/copy this"), None);
        assert_eq!(dispatch("/exit"), Some(("/quit", String::new())));
        assert_eq!(dispatch("/resources"), Some(("/resources", String::new())));
        assert_eq!(dispatch("/context"), Some(("/context", String::new())));
    }

    #[test]
    fn export_consumes_its_path_argument() {
        assert_eq!(
            dispatch("/export ~/notes.md"),
            Some(("/export", "~/notes.md".to_string()))
        );
        assert_eq!(
            dispatch("/export   /tmp/a.txt  "),
            Some(("/export", "/tmp/a.txt".to_string()))
        );
        assert_eq!(dispatch("/rewind 7"), Some(("/rewind", "7".to_string())));
        // A bare command matches; its handler turns the empty argument into the
        // usage hint.
        assert_eq!(dispatch("/export"), Some(("/export", String::new())));
    }

    #[test]
    fn focus_consumes_its_name_argument() {
        assert_eq!(
            dispatch("/focus scout"),
            Some(("/focus", "scout".to_string()))
        );
        assert_eq!(dispatch("/focus"), Some(("/focus", String::new())));
    }

    #[test]
    fn branch_matches_bare_and_with_prompt_and_close_resolves() {
        // An optional argument admits trailing text an argless one declines.
        assert_eq!(dispatch("/branch"), Some(("/branch", String::new())));
        assert_eq!(dispatch("/branch hi"), Some(("/branch", "hi".to_string())));
        assert_eq!(dispatch("/close"), Some(("/close", String::new())));
    }

    #[test]
    fn unknown_token_is_not_a_command() {
        assert_eq!(dispatch("/bogus"), None);
        assert_eq!(dispatch("just a prompt"), None);
    }

    #[test]
    fn unrecognized_command_flags_only_a_slash_typo() {
        assert_eq!(unrecognized_command("/bogus"), Some("/bogus"));
        assert_eq!(
            unrecognized_command("/bad_command here are the argv"),
            Some("/bad_command")
        );
        // A real command misused with trailing text is a deliberate fall-through
        // to the model, not a typo.
        assert_eq!(unrecognized_command("/copy this"), None);
        assert_eq!(unrecognized_command("just a prompt"), None);
    }

    #[test]
    fn a_bare_slash_offers_every_command_and_alias() {
        let all: usize = SLASH_COMMANDS.iter().map(|c| c.aliases.len() + 1).sum();
        assert_eq!(replacements("/").len(), all);
    }

    #[test]
    fn a_prefix_narrows_and_an_alias_stands_for_itself() {
        assert_eq!(replacements("/thin"), ["/thinking"]);
        assert!(replacements("/ex").contains(&"/exit".to_string()));
    }

    #[test]
    fn a_typed_space_or_a_plain_line_ends_the_completion() {
        assert!(replacements("/export ").is_empty());
        assert!(replacements("/export ~/notes.md").is_empty());
        assert!(replacements("what is a monad").is_empty());
    }

    #[test]
    fn the_argument_hint_shows_but_is_never_spliced() {
        let export = command_candidates("/export")
            .into_iter()
            .find(|c| c.replacement == "/export")
            .expect("/export completes itself");
        assert_eq!(export.display, "/export <path>");
        assert_eq!(
            export.detail.as_deref(),
            Some("Write the user view to a file.")
        );
    }

    // Twins rather than one genericised test: absoluteness is host-defined
    // (`/tmp/out.txt` is not absolute on Windows), so each host pins its own.
    #[cfg(unix)]
    #[test]
    fn export_path_resolves_absolute_and_relative() {
        assert_eq!(
            resolve_export_path("/tmp/out.txt", "/Users/me/proj").to_str(),
            Some("/tmp/out.txt")
        );
        assert_eq!(
            resolve_export_path("notes.md", "/Users/me/proj").to_str(),
            Some("/Users/me/proj/notes.md")
        );
    }

    #[cfg(windows)]
    #[test]
    fn export_path_resolves_absolute_and_relative() {
        assert_eq!(
            resolve_export_path(r"C:\scratch\out.txt", r"C:\Users\me\proj").to_str(),
            Some(r"C:\scratch\out.txt")
        );
        assert_eq!(
            resolve_export_path("notes.md", r"C:\Users\me\proj").to_str(),
            Some(r"C:\Users\me\proj\notes.md")
        );
    }
}
