//! Prompt-box slash commands — registry, routing, and handlers.

use std::fmt::Write;
use std::path::PathBuf;

use super::App;
use super::app::CommandCtx;
use super::block::Chrome;
use super::export::{Format, export_to};
use super::gesture::Toast;
use super::login;
use super::model_picker::pick_model;
use super::providers;
use super::rewind;
use crate::SessionInfo;
use crate::agent::Agent;
use crate::agent::fleet::Fleet;
use crate::bus::{Post, Read, Rewrite};
use crate::card::{Card, Field, FieldVal, Mark, Readout, Span};
use crate::provider::allowance::{Allowance, Reading};
use crate::record::{Memory, RECORD};
use jiff::Timestamp;
use prompt_editor::completion::Candidate;
use ral_core::process::CancelCause;

/// A command's trailing argument.
pub(super) enum Arg {
    No,
    Optional(&'static str),
    Required(&'static str),
}

pub(super) struct Spec {
    pub(super) name: &'static str,
    pub(super) aliases: &'static [&'static str],
    pub(super) arg: Arg,
    pub(super) help: &'static str,
    /// Builds the command from its argument, already checked against `arg`.
    command: fn(&str) -> Command,
}

/// The slash commands, in `/help` order: the one list behind the highlight,
/// the completion, the listing and the routing.
const COMMANDS: &[Spec] = &[
    Spec {
        name: "/help",
        aliases: &[],
        arg: Arg::No,
        help: "List the available commands.",
        command: |_| Command::Help,
    },
    Spec {
        name: "/legend",
        aliases: &[],
        arg: Arg::No,
        help: "Decode the rail, bars, grain, and fidelity treatments.",
        command: |_| Command::Legend,
    },
    Spec {
        name: "/thinking",
        aliases: &[],
        arg: Arg::No,
        help: "Collapse or expand thinking, on screen and to come.",
        command: |_| Command::Thinking,
    },
    Spec {
        name: "/clear",
        aliases: &[],
        arg: Arg::No,
        help: "Forget the conversation and clear the screen.",
        command: |_| Command::Clear,
    },
    Spec {
        name: "/copy",
        aliases: &[],
        arg: Arg::No,
        help: "Copy the latest reply to the clipboard.",
        command: |_| Command::Copy,
    },
    Spec {
        name: "/export",
        aliases: &[],
        arg: Arg::Required("<path>"),
        help: "Write the user view to a file.",
        command: |a| Command::Export(a.to_owned()),
    },
    Spec {
        name: "/model",
        aliases: &[],
        arg: Arg::No,
        help: "Switch the model or provider.",
        command: |_| Command::Model,
    },
    Spec {
        name: "/login",
        aliases: &[],
        arg: Arg::No,
        help: "Sign in to a plan: ChatGPT, Codex or Grok.",
        command: |_| Command::Login,
    },
    Spec {
        name: "/providers",
        aliases: &[],
        arg: Arg::No,
        help: "Manage providers: API keys, custom endpoints, plan sign-ins.",
        command: |_| Command::Providers,
    },
    Spec {
        name: "/limits",
        aliases: &[],
        arg: Arg::No,
        help: "Show what is left of each subscription's ration.",
        command: |_| Command::Limits,
    },
    Spec {
        name: "/new",
        aliases: &[],
        arg: Arg::Optional("[name]"),
        help: "Open a new, empty session.",
        command: |a| Command::Hatch(Memory::Amnemon, (!a.is_empty()).then(|| a.to_owned())),
    },
    Spec {
        name: "/branch",
        aliases: &[],
        arg: Arg::Optional("[name]"),
        help: "Open a new session with this tab's context (fresh shell).",
        command: |a| Command::Hatch(Memory::Mnemon, (!a.is_empty()).then(|| a.to_owned())),
    },
    Spec {
        name: "/close",
        aliases: &[],
        arg: Arg::No,
        help: "End this session for good: --resume will not reopen it.",
        command: |_| Command::Close,
    },
    Spec {
        name: "/focus",
        aliases: &[],
        arg: Arg::Required("<name>"),
        help: "Attach to a live agent by name.",
        command: |a| Command::Focus(a.to_owned()),
    },
    Spec {
        name: "/evict",
        aliases: &[],
        arg: Arg::No,
        help: "Evict the older half of the context; it stays readable to the model.",
        command: |_| Command::Evict,
    },
    Spec {
        name: "/context",
        aliases: &[],
        arg: Arg::No,
        help: "Survey the model context without changing it.",
        command: |_| Command::Context,
    },
    Spec {
        name: "/rewind",
        aliases: &[],
        arg: Arg::No,
        help: "Pick a prompt to go back to; descendants and the shell are untouched.",
        command: |_| Command::Rewind,
    },
    Spec {
        name: "/resources",
        aliases: &[],
        arg: Arg::No,
        help: "Show the agent's resource probes: workers, inbox, log, disk.",
        command: |_| Command::Resources,
    },
    Spec {
        name: "/quit",
        aliases: &["/exit"],
        arg: Arg::No,
        help: "End every session; --resume reopens them.",
        command: |_| Command::Quit,
    },
];

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
    /// `/new` and `/branch`: the focused agent opens a root session of its
    /// fleet.
    Hatch(Memory, Option<String>),
    Close,
    Focus(String),
    Evict,
    Context,
    Rewind,
    Resources,
    Quit,
}

/// The command a token names, by its own name or by one of its aliases.
fn by_token(token: &str) -> Option<&'static Spec> {
    COMMANDS
        .iter()
        .find(|c| c.name == token || c.aliases.contains(&token))
}

/// The command named by `trimmed`'s first token, with the trimmed remainder as
/// its argument.  An argument-less command matches only when typed alone, so
/// `/copy this` declines and the line proceeds to the model as a prompt.
pub(super) fn lookup_command(trimmed: &str) -> Option<(&'static Spec, &str)> {
    let (head, rest) = split_head(trimmed);
    let spec = by_token(head)?;
    if matches!(spec.arg, Arg::No) && !rest.is_empty() {
        return None;
    }
    Some((spec, rest))
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
    let tokens: Vec<&'static str> = COMMANDS
        .iter()
        .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied()))
        .collect();
    ral_core::text::rank(line, tokens)
        .into_iter()
        .map(|token| {
            let spec = by_token(token);
            Candidate {
                display: match spec.map(|c| &c.arg) {
                    Some(Arg::Optional(hint) | Arg::Required(hint)) => format!("{token} {hint}"),
                    _ => token.to_string(),
                },
                detail: spec.map(|c| c.help.to_string()),
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
pub(super) fn resolve_export_path(arg: &str, cwd: &ral_core::path::CwdPath) -> PathBuf {
    ral_core::path::Resolver::at(&ral_core::host::EnvVars::new(), cwd)
        .resolve(arg)
        .into_inner()
}

/// The registry as a framed card: one `(command, gloss)` row per entry, so the
/// listing is one block with the card's own column alignment rather than a
/// column of notes.
pub(super) fn cmd_help(app: &mut App) {
    let rows = COMMANDS
        .iter()
        .map(|c| {
            let mut label = c.name.to_string();
            if let Arg::Optional(hint) | Arg::Required(hint) = c.arg {
                let _ = write!(label, " {hint}");
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
    app.push_chrome(app.tabs.focused(), Chrome::Framed(card));
}

pub(super) fn cmd_legend(app: &mut App) {
    app.push_chrome(app.tabs.focused(), Chrome::Legend);
}

/// Flip the disclosure of deliberation everywhere at once: one setting, so a
/// group already on screen and one that arrives an hour from now read alike.
/// The repaint is the answer; no note restates it.
pub(super) fn cmd_thinking(app: &mut App) {
    app.tabs.toggle_thinking();
}

/// Copy the latest reply, as raw markdown, to the clipboard; only over OSC 52
/// does a long one arrive tail-first, announced.  The outcome is a corner
/// toast, as a drag-selection's copy is: a copy is a gesture on the transcript,
/// not an event in it.
pub(super) fn cmd_copy(app: &mut App) {
    let id = app.tabs.focused();
    let reply = app.latest_reply();
    if reply.is_empty() {
        app.push_error(id, "no reply to copy yet");
        return;
    }
    let sent = app.clipboard.copy(&reply, true).ok();
    app.gesture
        .note_copy(sent, Toast::Reply(reply.lines().count()));
}

/// Render the focused tab's `record.jsonl` to `arg`, never over an existing
/// file.
pub(super) fn cmd_export(app: &mut App, arg: &str, info: &SessionInfo<'_>) {
    let id = app.tabs.focused();
    let dest = match ral_core::path::CwdPath::new(info.cwd.to_path_buf()) {
        Ok(cwd) => resolve_export_path(arg, &cwd),
        Err(e) => {
            app.push_error(id, &e.to_string());
            return;
        }
    };
    let log_dir = app.tabs.focused_log_dir();
    let exported = log_dir
        .open(RECORD)
        .map_err(std::io::Error::from)
        .and_then(|record| export_to(record, &log_dir.path().join(RECORD), &dest, Format::Plain));
    match exported {
        Ok(()) => app.push_note(
            id,
            &format!("[exported transcript to {}]", ral_core::path::shown(&dest)),
        ),
        Err(e) => app.push_error(id, &e.to_string()),
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
pub(super) fn cmd_limits(app: &mut App, ctx: &CommandCtx<'_>) {
    let id = app.tabs.focused();
    match ctx.bureau.survey_allowances() {
        None => app.push_error(
            id,
            "this session replays a scripted provider and surveys no accounts",
        ),
        Some(survey) => {
            let jobs = app.jobs.clone();
            std::thread::spawn(move || {
                let card = limits_card(&survey.settle());
                let _ = jobs.send(Box::new(move |app, _| {
                    if !app.tabs.lingering(id) {
                        app.push_chrome(id, Chrome::Framed(card));
                    }
                }));
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
/// focused tab.  A plain line steers the focused tab's agent.  Errors land on
/// the focused tab, where the user typed.
pub(super) fn route_submit(text: String, app: &mut App, ctx: &CommandCtx<'_>) {
    let trimmed = text.trim();
    let focused = app.tabs.focused();
    match lookup_command(trimmed) {
        Some((spec, arg)) => match spec.arg {
            Arg::Required(hint) if arg.is_empty() => {
                app.push_error(focused, &format!("usage: {} {hint}", spec.name));
            }
            _ => run((spec.command)(arg), app, ctx),
        },
        // A typo is not a prompt in disguise: say so rather than mail it to the
        // model as one.
        None => match unrecognized_command(trimmed) {
            Some(head) => app.push_error(focused, &format!("unknown command: {head} (see /help)")),
            // `steer` is the one delivery path: it renews the agent's idle
            // lease, and the line is dropped if that agent died since it was
            // focused.
            None => {
                if let Some(agent) = app.tabs.agent(focused) {
                    agent.mailbox.steer(text);
                }
            }
        },
    }
}

/// Interrupt `agent`'s exchange, then queue `rewrite` behind it.
fn end(agent: &Agent, rewrite: Rewrite) {
    agent.interrupt();
    agent.mailbox.push(Post::Rewrite(rewrite));
}

/// A retained parent identity need not have an inbox consumer.
pub(super) fn quit(fleet: &Fleet) {
    for root in fleet.roots() {
        root.cancel_tree(CancelCause::Cancelled);
    }
}

/// Act on a well-formed command.  A view command (`/help`, `/legend`, `/copy`,
/// `/export`, `/model`, `/login`, `/limits`, `/thinking`) touches only the App,
/// clipboard, file, or picker, so it runs here on the UI thread; `/quit`
/// cancels the fleet directly. A session command becomes its [`Read`] or
/// [`Rewrite`] and rides the focused agent's
/// inbox to its attend thread, which owns the context.  `/rewind` is both: its
/// picker runs here, and the choice rides the inbox.
fn run(command: Command, app: &mut App, ctx: &CommandCtx<'_>) {
    match command {
        Command::Focus(name) => cmd_focus(app, &name),
        Command::Thinking => cmd_thinking(app),
        Command::Help => cmd_help(app),
        Command::Legend => cmd_legend(app),
        Command::Copy => cmd_copy(app),
        Command::Export(path) => cmd_export(app, &path, ctx.info),
        Command::Model => pick_model(app, ctx),
        Command::Login => login::login(app),
        Command::Providers => providers::providers(app, ctx),
        Command::Limits => cmd_limits(app, ctx),
        Command::Clear => app.session(|agent| end(agent, Rewrite::Clear)),
        Command::Close => app.session(|agent| end(agent, Rewrite::Close)),
        Command::Quit => quit(app.tabs.fleet()),
        Command::Evict => app.post(Post::Rewrite(Rewrite::Evict)),
        Command::Rewind => rewind::rewind(app),
        Command::Hatch(memory, name) => app.post(Post::Read(Read::Hatch { memory, name })),
        Command::Context => app.post(Post::Read(Read::Context)),
        Command::Resources => app.post(Post::Read(Read::Resources)),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
