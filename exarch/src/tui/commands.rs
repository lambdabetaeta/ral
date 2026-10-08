//! Prompt-box slash commands — registry, routing, and handlers.

use std::fmt::Write;
use std::io;
use std::path::PathBuf;

use super::App;
use super::block::Chrome;
use super::gesture::Toast;
use super::login;
use super::model_picker::pick_model;
use super::providers;
use super::rewind;
use super::scrollback;
use super::terminal::{YANK_CAP, osc52_copy, tail_bytes};
use super::tui_loop::Tui;
use crate::SessionInfo;
use crate::bus::{Mailbox, Post, Read, Rewrite};
use crate::card::{Card, Field, FieldVal, Mark, Readout, Span};
use crate::provider::allowance::{Allowance, Reading};
use jiff::Timestamp;
use prompt_editor::completion::Candidate;
use ral_core::path::sigil::expand_path_prefix;
/// The slash commands, by name.  The one list behind the prompt-box
/// highlight, the completion, the `/help` listing, and the routing: adding a
/// command is adding a variant, and [`Verb::meta`], [`Verb::parse`] and
/// [`run`] each match it exhaustively.
macro_rules! verbs {
    ($($verb:ident),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub(super) enum Verb { $($verb),* }
        impl Verb {
            pub(super) const ALL: &'static [Verb] = &[$(Verb::$verb),*];
        }
    };
}

verbs! {
    Help, Legend, Thinking, Clear, Copy, Export, Model, Login, Providers, Limits, Branch,
    Close, Focus, Evict, Context, Rewind, Resources, Quit,
}

pub(super) struct Meta {
    pub(super) name: &'static str,
    pub(super) aliases: &'static [&'static str],
    /// The trailing argument, e.g. `Some("<path>")` for `/export`; `None` marks
    /// a command that matches only when typed alone.
    pub(super) arg: Option<&'static str>,
    /// Whether the command runs wherever it is typed.  A command that reaches
    /// the session inbox belongs to the trunk's context and is refused off
    /// it; one that touches only the view runs on any tab.
    pub(super) any_tab: bool,
    pub(super) help: &'static str,
}

const fn meta(
    name: &'static str,
    aliases: &'static [&'static str],
    arg: Option<&'static str>,
    any_tab: bool,
    help: &'static str,
) -> Meta {
    Meta {
        name,
        aliases,
        arg,
        any_tab,
        help,
    }
}

impl Verb {
    pub(super) const fn meta(self) -> Meta {
        match self {
            Self::Help => meta("/help", &[], None, false, "List the available commands."),
            Self::Legend => meta(
                "/legend",
                &[],
                None,
                false,
                "Decode the rail, bars, grain, and fidelity treatments.",
            ),
            Self::Thinking => meta(
                "/thinking",
                &[],
                None,
                true,
                "Collapse or expand thinking, on screen and to come.",
            ),
            Self::Clear => meta(
                "/clear",
                &[],
                None,
                false,
                "Forget the conversation and clear the screen.",
            ),
            Self::Copy => meta(
                "/copy",
                &[],
                None,
                false,
                "Copy the latest reply to the clipboard.",
            ),
            Self::Export => meta(
                "/export",
                &[],
                Some("<path>"),
                false,
                "Write the user view to a file.",
            ),
            Self::Model => meta("/model", &[], None, false, "Switch the model or provider."),
            Self::Login => meta(
                "/login",
                &[],
                None,
                false,
                "Sign in with ChatGPT: adds a plan-backed provider.",
            ),
            Self::Providers => meta(
                "/providers",
                &[],
                None,
                false,
                "Manage providers: API keys, custom endpoints, ChatGPT sign-in.",
            ),
            Self::Limits => meta(
                "/limits",
                &[],
                None,
                false,
                "Show what is left of each subscription's ration.",
            ),
            Self::Branch => meta(
                "/branch",
                &[],
                Some("[name]"),
                false,
                "Fork this conversation into a new tab (same context).",
            ),
            Self::Close => meta(
                "/close",
                &[],
                None,
                true,
                "Close this branch (its tab and any agents it spawned).",
            ),
            Self::Focus => meta(
                "/focus",
                &[],
                Some("<name>"),
                true,
                "Attach to a live agent by name.",
            ),
            Self::Evict => meta(
                "/evict",
                &[],
                None,
                false,
                "Evict the older half of the context; it stays readable to the model.",
            ),
            Self::Context => meta(
                "/context",
                &[],
                None,
                false,
                "Survey the model context without changing it.",
            ),
            Self::Rewind => meta(
                "/rewind",
                &[],
                None,
                false,
                "Pick a prompt to go back to; descendants and the shell are untouched.",
            ),
            Self::Resources => meta(
                "/resources",
                &[],
                None,
                false,
                "Show the agent's resource probes: workers, inbox, log, disk.",
            ),
            Self::Quit => meta("/quit", &["/exit"], None, false, "Leave exarch."),
        }
    }

    /// Type the trailing argument, or say how it is malformed — the usage
    /// hints live here, so [`run`] receives only well-formed commands.
    fn parse(self, arg: &str) -> Result<Command, String> {
        Ok(match self {
            Self::Help => Command::Help,
            Self::Legend => Command::Legend,
            Self::Thinking => Command::Thinking,
            Self::Clear => Command::Clear,
            Self::Copy => Command::Copy,
            Self::Model => Command::Model,
            Self::Login => Command::Login,
            Self::Providers => Command::Providers,
            Self::Limits => Command::Limits,
            Self::Close => Command::Close,
            Self::Evict => Command::Evict,
            Self::Context => Command::Context,
            Self::Resources => Command::Resources,
            Self::Rewind => Command::Rewind,
            Self::Quit => Command::Quit,
            Self::Branch => Command::Branch((!arg.is_empty()).then(|| arg.to_string())),
            Self::Export if arg.is_empty() => return Err("usage: /export <path>".into()),
            Self::Export => Command::Export(arg.to_string()),
            Self::Focus if arg.is_empty() => return Err("usage: /focus <name>".into()),
            Self::Focus => Command::Focus(arg.to_string()),
        })
    }
}

/// A typed command, ready to run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Command {
    Help,
    Legend,
    Thinking,
    Clear,
    Copy,
    Export(String),
    Model,
    Login,
    Providers,
    Limits,
    Branch(Option<String>),
    Close,
    Focus(String),
    Evict,
    Context,
    Rewind,
    Resources,
    Quit,
}

/// The command a token names, by its own name or by one of its aliases.
fn by_token(token: &str) -> Option<Verb> {
    Verb::ALL.iter().copied().find(|v| {
        let m = v.meta();
        m.name == token || m.aliases.contains(&token)
    })
}

/// The command named by `trimmed`'s first token, with the trimmed remainder as
/// its argument.  An argument-less command matches only when typed alone, so
/// `/copy this` declines and the line proceeds to the model as a prompt.
pub(super) fn lookup_command(trimmed: &str) -> Option<(Verb, &str)> {
    let (head, rest) = split_head(trimmed);
    let verb = by_token(head)?;
    if verb.meta().arg.is_none() && !rest.is_empty() {
        return None;
    }
    Some((verb, rest))
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
    let tokens: Vec<&'static str> = Verb::ALL
        .iter()
        .map(|v| v.meta())
        .flat_map(|m| std::iter::once(m.name).chain(m.aliases.iter().copied()))
        .collect();
    ral_core::text::rank(line, tokens, false)
        .into_iter()
        .map(|token| {
            let meta = by_token(token).map(Verb::meta);
            Candidate {
                display: match meta.as_ref().and_then(|m| m.arg) {
                    Some(arg) => format!("{token} {arg}"),
                    None => token.to_string(),
                },
                detail: meta.map(|m| m.help.to_string()),
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

/// Split off the first whitespace-delimited token — the head/rest shape
/// [`lookup_command`] parses into.
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

/// The registry as a framed card: one `(command, gloss)` row per entry, so the
/// listing is one block with the card's own column alignment rather than a
/// column of notes.
pub(super) fn cmd_help(app: &mut App) {
    let rows = Verb::ALL
        .iter()
        .map(|v| v.meta())
        .map(|c| {
            let mut label = c.name.to_string();
            if let Some(arg) = c.arg {
                let _ = write!(label, " {arg}");
            }
            if !c.aliases.is_empty() {
                let _ = write!(label, " ({})", c.aliases.join(", "));
            }
            Field {
                label,
                value: FieldVal::Inline(vec![Span::plain(c.help)]),
            }
        })
        .collect();
    let card = Card(vec![Mark::heading("commands"), Mark::Fields { rows }]);
    app.push_chrome(app.tabs.root(), Chrome::Framed(card));
}

pub(super) fn cmd_legend(app: &mut App) {
    app.push_chrome(app.tabs.root(), Chrome::Legend);
}

/// Flip the disclosure of deliberation everywhere at once: one setting, so a
/// group already on screen and one that arrives an hour from now read alike.
/// The repaint is the answer; no note restates it.
pub(super) fn cmd_thinking(app: &mut App) {
    app.tabs.toggle_thinking();
}

/// Copy the latest reply, as raw markdown, to the clipboard via OSC 52.  A reply
/// past the terminal's per-sequence limit is copied tail-first and announced,
/// since the terminal would otherwise drop the sequence and copy nothing.  The
/// outcome is a corner toast, as a drag-selection's copy is: a copy is a
/// gesture on the transcript, not an event in it.
pub(super) fn cmd_copy(app: &mut App) {
    let id = app.tabs.root();
    let reply = app.latest_reply();
    if reply.is_empty() {
        app.push_error(id, "no reply to copy yet");
        return;
    }
    let payload = tail_bytes(&reply, YANK_CAP);
    let toast = if osc52_copy(payload).is_err() {
        Toast::CopyFailed
    } else if payload.len() < reply.len() {
        Toast::ReplyTail(payload.len())
    } else {
        Toast::Reply(reply.lines().count())
    };
    app.gesture.note(toast);
}

/// Write the focused tab's rendered `user.log` to `arg`, never over an existing
/// file.  The copy goes through [`scrollback::export_log`], the I/O door.
pub(super) fn cmd_export(app: &mut App, arg: &str, info: &SessionInfo<'_>) {
    let id = app.tabs.root();
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
    match scrollback::export_log(&src, &dest) {
        Ok(_) => app.push_note(id, &format!("[exported user view to {}]", dest.display())),
        Err(e) => app.push_error(id, &format!("could not write {}: {e}", dest.display())),
    }
}

/// Attach to the live tab named `arg`. The name resolves, but nothing is
/// renewed: attention alone must not keep a child alive.
pub(super) fn cmd_focus(app: &mut App, arg: &str) {
    let id = app.tabs.focused();
    match app.tabs.by_name(arg) {
        Some(target) => app.tabs.set_focus(target),
        None => app.push_error(id, &format!("no live tab named {arg}")),
    }
}

/// Survey every account's ration on a background thread, so the card lands
/// whole or not at all — a per-account failure is already a row on it.
pub(super) fn cmd_limits(app: &mut App, ctx: &super::tui_loop::CommandCtx<'_>) {
    let id = app.tabs.root();
    match ctx.bureau.survey_allowances() {
        None => app.push_error(
            id,
            "this session replays a scripted provider and surveys no accounts",
        ),
        Some(survey) => {
            let recorder = ctx.recorder.clone();
            std::thread::spawn(move || {
                let card = limits_card(&survey.settle());
                recorder.transient(crate::record::Transient::Limits { card });
            });
        }
    }
}

/// One section per account — its label as the heading — over its rows.
///
/// Unmetered accounts draw no section; when every account is one, the card
/// names them in a sentence rather than reading as a claim of no limit.
fn limits_card(readings: &[(String, Reading)]) -> Card {
    let now = Timestamp::now();
    if readings
        .iter()
        .all(|(_, r)| matches!(r, Reading::Unmetered))
    {
        let names = readings
            .iter()
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let sentence = if readings.is_empty() {
            "there are no accounts to check for a ration".to_string()
        } else {
            format!(
                "none of {names} publishes a ration: /limits reports subscriptions and credit balances"
            )
        };
        return Card(vec![Mark::Text {
            spans: vec![Span::plain(sentence)],
        }]);
    }
    let mut marks = Vec::new();
    for (label, reading) in readings {
        let mark = match reading {
            Reading::Allowances(allowances) => {
                let mut sorted: Vec<&Allowance> = allowances.iter().collect();
                sorted.sort_by_key(|a| (a.window.is_none(), a.window));
                Mark::Fields {
                    rows: sorted.iter().map(|a| allowance_field(a, now)).collect(),
                }
            }
            Reading::Unmetered => continue,
            Reading::Failed(reason) => Mark::Text {
                spans: vec![Span::plain(format!("{label}: {reason}"))],
            },
        };
        marks.push(Mark::heading(label));
        marks.push(mark);
    }
    Card(marks)
}

/// One allowance as an aligned row: a bar when its share is known, its raw
/// figures otherwise.
fn allowance_field(a: &Allowance, now: Timestamp) -> Field {
    let value = match a.percent() {
        Some(value) => FieldVal::Readout(Readout {
            value,
            max: Some(100),
            unit: Some("%".into()),
        }),
        None => FieldVal::Inline(vec![Span::plain(a.figure_text())]),
    };
    Field {
        label: a.label_at(now),
        value,
    }
}

/// The one submit path for every tab: parse once, then act on the parse and the
/// focused tab.  A command typed on a sub-agent tab is refused rather than
/// misfired — the trunk's inbox would act on the wrong session — save those
/// the registry marks `any_tab`, which touch no inbox.  A plain line steers
/// the focused tab instead.  Errors land on the focused tab, where the user
/// typed.
pub(super) fn route_submit(
    text: String,
    tui: &mut Tui,
    mailbox: &Mailbox,
    ctx: &super::tui_loop::CommandCtx<'_>,
) -> io::Result<()> {
    let trimmed = text.trim();
    let root = tui.app.tabs.root();
    let focused = tui.app.tabs.focused();
    let unrecognized = unrecognized_command(trimmed);
    match lookup_command(trimmed) {
        Some((verb, _)) if focused != root && !verb.meta().any_tab => {
            tui.app.push_error(
                focused,
                &format!("{} is not available on this tab", verb.meta().name),
            );
        }
        Some((verb, arg)) => match verb.parse(arg) {
            Ok(command) => run(command, tui, mailbox, ctx)?,
            Err(usage) => tui.app.push_error(focused, &usage),
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
                mailbox.push_user(text);
            } else if let Some(agent) = tui.app.tabs.agent(focused) {
                agent.mailbox.steer(text);
            }
        }
    }
    Ok(())
}

/// Act on a well-formed command.  A view command (`/help`, `/legend`, `/copy`,
/// `/export`, `/model`, `/login`, `/limits`, `/thinking`) touches only the App,
/// clipboard, file, or picker, so it runs here on the UI thread; a session
/// command becomes its [`Read`] or [`Rewrite`] and rides the trunk's inbox to
/// the attend thread, which owns the context.  `/rewind` is both: its picker
/// runs here, and the choice rides the inbox.
fn run(
    command: Command,
    tui: &mut Tui,
    mailbox: &Mailbox,
    ctx: &super::tui_loop::CommandCtx<'_>,
) -> io::Result<()> {
    let info = ctx.info;
    let root = tui.app.tabs.root();
    let focused = tui.app.tabs.focused();
    match command {
        Command::Close => {
            if focused == root {
                tui.app
                    .push_error(root, "nothing to close here; /quit ends the session");
            } else if !tui.app.tabs.is_branch(focused) {
                tui.app
                    .push_error(focused, "/close closes a branch, not this tab");
            } else if let Some(agent) = tui.app.tabs.focused_agent() {
                agent.cancel_tree(ral_core::process::CancelCause::Cancelled);
            } else {
                tui.app.push_error(
                    focused,
                    "this branch has already ended; its tab fades on its own",
                );
            }
        }
        Command::Focus(name) => cmd_focus(&mut tui.app, &name),
        Command::Thinking => cmd_thinking(&mut tui.app),
        Command::Help => cmd_help(&mut tui.app),
        Command::Legend => cmd_legend(&mut tui.app),
        Command::Copy => cmd_copy(&mut tui.app),
        Command::Export(path) => cmd_export(&mut tui.app, &path, info),
        Command::Model => pick_model(tui, ctx),
        Command::Login => login::login(tui, ctx),
        Command::Providers => providers::providers(tui, ctx),
        Command::Limits => cmd_limits(&mut tui.app, ctx),
        // Cancel before blanking: tokens already in flight would otherwise
        // paint into the cleared scrollback until the worker's next poll, and
        // what the bus still holds `App::handle`'s clear-drain drops.
        // Descendants only — a terminate-class cause on the trunk's own
        // token is permanent, and `/clear` rebuilds the trunk in place.
        // The pre-blank cancel reaches a foreground external child too.
        Command::Clear => {
            crate::signals::raise_interrupt();
            if let Some(agent) = tui.app.tabs.agent(root) {
                agent.interrupt();
                agent.cancel_descendants(ral_core::process::CancelCause::Cancelled);
            }
            tui.app.clear(info, tui.guard.term())?;
            mailbox.push(Post::Rewrite(Rewrite::Clear));
        }
        Command::Evict => mailbox.push(Post::Rewrite(Rewrite::Evict)),
        Command::Quit => mailbox.push(Post::Rewrite(Rewrite::Quit)),
        Command::Rewind => rewind::rewind(tui, mailbox),
        Command::Branch(name) => mailbox.push(Post::Read(Read::Branch(name))),
        Command::Context => mailbox.push(Post::Read(Read::Context)),
        Command::Resources => mailbox.push(Post::Read(Read::Resources)),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
