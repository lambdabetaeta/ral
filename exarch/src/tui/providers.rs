//! The `/providers` overlay — every account, and the keys, endpoints and
//! sign-ins behind them, changed from inside a running session.
//!
//! [`ProvidersOverlay`] is display and input only; [`providers`] drives it and
//! applies each [`Action`] through the [`Wallet`], live in the store and
//! catalog as well as on disk — the same split `login.rs` makes.

use super::app::Overlay;
use super::line;
use super::login;
use super::palette::{CYAN, Col, LIME, ORANGE, OVERLAY_BG, PURPLE, RED, SLATE};
use super::picker::{PAD_X, PAD_Y, centered, overlay_frame};
use super::tui_loop::{CommandCtx, OverlayTick, Tui, overlay_tick};
use crate::provider::Holdings;
use crate::provider::accounts::{Entry, Source, entries};
use crate::provider::identity;
use crate::provider::oauth;
use crate::wallet::Wallet;
use ral_core::sync::LockExt;
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::mpsc;
use std::thread;

const OVERLAY_W: u16 = 104;
const BODY_W: usize = OVERLAY_W as usize - 2 - 2 * PAD_X as usize;
const HEADS: [&str; 3] = ["SERVICE", "ENDPOINT / KIND", "KEY"];
const FORM: [&str; 4] = ["name", "address", "protocol", "key"];
const PROTOCOL: usize = 2;
const KEY: usize = 3;
const FORM_LABEL: Col = Col::wide(9);
const HOT: Style = Style::new().fg(CYAN).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(SLATE).add_modifier(Modifier::DIM);

/// One line of text; a masked one shows a bullet per character.
#[derive(Default)]
struct Field {
    text: String,
    masked: bool,
}

impl Field {
    fn insert(&mut self, c: char) {
        if !c.is_control() {
            self.text.push(c);
        }
    }

    fn paste(&mut self, text: &str) {
        text.trim().chars().for_each(|c| self.insert(c));
    }

    fn shown(&self) -> String {
        if self.masked {
            "•".repeat(self.text.chars().count())
        } else {
            self.text.clone()
        }
    }
}

/// An endpoint being declared: name, address, protocol, key.
struct Form {
    fields: [Field; 4],
    focus: usize,
    /// The server checks no key, and the key field is set aside.
    keyless: bool,
}

impl Form {
    fn new() -> Self {
        let field = |text: &str, masked| Field {
            text: text.to_string(),
            masked,
        };
        Self {
            fields: [
                field("", false),
                field("", false),
                field(identity::protocols()[0], false),
                field("", true),
            ],
            focus: 0,
            keyless: false,
        }
    }

    /// The field typing reaches: not the protocol, which cycles, nor a key
    /// the server will not check.
    fn typing(&mut self) -> Option<&mut Field> {
        match self.focus {
            PROTOCOL => None,
            KEY if self.keyless => None,
            at => Some(&mut self.fields[at]),
        }
    }

    fn key(&mut self, code: KeyCode) -> Action {
        let n = FORM.len();
        match code {
            KeyCode::Enter if self.focus == KEY => return self.submit(),
            KeyCode::Tab | KeyCode::Down | KeyCode::Enter => self.focus = (self.focus + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.focus = (self.focus + n - 1) % n,
            KeyCode::Left | KeyCode::Right if self.focus == PROTOCOL => {
                self.cycle(code == KeyCode::Right);
            }
            KeyCode::Left | KeyCode::Right if self.focus == KEY => self.keyless = !self.keyless,
            _ => {}
        }
        Action::None
    }

    fn cycle(&mut self, forward: bool) {
        let all = identity::protocols();
        let at = all
            .iter()
            .position(|p| *p == self.fields[PROTOCOL].text)
            .unwrap_or(0);
        let next = (at + if forward { 1 } else { all.len() - 1 }) % all.len();
        self.fields[PROTOCOL].text = all[next].to_string();
    }

    fn submit(&self) -> Action {
        let [name, address, protocol, key] = &self.fields;
        Action::Change(Change::AddEndpoint {
            name: name.text.clone(),
            address: address.text.clone(),
            protocol: protocol.text.clone(),
            key: (!self.keyless).then(|| key.text.clone()),
        })
    }
}

enum Mode {
    List,
    Key {
        id: String,
        label: String,
        field: Field,
    },
    Form(Form),
    Confirm {
        ask: String,
        then: Action,
    },
}

/// A change to the wallet's holdings.
pub(super) enum Change {
    SetKey {
        id: String,
        key: String,
    },
    ForgetKey(String),
    /// `key` is `None` for a server that checks none.
    AddEndpoint {
        name: String,
        address: String,
        protocol: String,
        key: Option<String>,
    },
    Remove(String),
}

/// What a key press asks the driver to do.
pub(super) enum Action {
    None,
    Change(Change),
    SignOut(String),
    SignIn,
}

impl Change {
    /// What was done, to whom, and how it went.
    fn apply(
        self,
        wallet: &Wallet,
        holdings: &Holdings,
    ) -> (&'static str, String, Result<(), String>) {
        match self {
            Self::SetKey { id, key } => {
                let result = wallet.set_key(holdings, &id, &key);
                ("saved the key for", id, result)
            }
            Self::ForgetKey(id) => {
                let result = wallet.forget_key(holdings, &id);
                ("cleared the saved key for", id, result)
            }
            Self::AddEndpoint {
                name,
                address,
                protocol,
                key,
            } => {
                let result =
                    wallet.add_endpoint(holdings, &name, &address, &protocol, key.as_deref());
                ("added", name, result)
            }
            Self::Remove(id) => {
                let result = wallet.forget_endpoint(holdings, &id);
                ("removed", id, result)
            }
        }
    }
}

pub(super) struct ProvidersOverlay {
    entries: Vec<Entry>,
    vault: String,
    /// Over the entries, then the two rows that add something.
    cursor: usize,
    mode: Mode,
    status: Option<Result<String, String>>,
}

impl ProvidersOverlay {
    fn new(entries: Vec<Entry>, vault: String) -> Self {
        Self {
            entries,
            vault,
            cursor: 0,
            mode: Mode::List,
            status: None,
        }
    }

    fn label(&self, id: &str) -> String {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .map_or_else(|| id.to_string(), |e| e.label.clone())
    }

    /// Back out of a sub-mode; `true` when already on the list, to close.
    fn cancel(&mut self) -> bool {
        self.status = None;
        matches!(std::mem::replace(&mut self.mode, Mode::List), Mode::List)
    }

    /// The field typing and pasting reach, if one has the focus.
    fn editing(&mut self) -> Option<&mut Field> {
        match &mut self.mode {
            Mode::Key { field, .. } => Some(field),
            Mode::Form(form) => form.typing(),
            _ => None,
        }
    }

    fn paste(&mut self, text: &str) {
        if let Some(field) = self.editing() {
            field.paste(text);
        }
    }

    /// Ctrl-S: submit the form.
    fn save(&self) -> Action {
        match &self.mode {
            Mode::Form(form) => form.submit(),
            _ => Action::None,
        }
    }

    /// Show `result`, and on success return to the list over the `fresh` table.
    fn settled(&mut self, result: Result<String, String>, fresh: Vec<Entry>) {
        if result.is_ok() {
            self.mode = Mode::List;
            self.entries = fresh;
            self.cursor = self.cursor.min(self.entries.len() + 1);
        }
        self.status = Some(result);
    }

    fn key(&mut self, code: KeyCode) -> Action {
        self.status = None;
        match std::mem::replace(&mut self.mode, Mode::List) {
            Mode::Confirm { then, .. } if code == KeyCode::Char('y') => return then,
            Mode::Confirm { .. } => return Action::None,
            mode => self.mode = mode,
        }
        if let Some(field) = self.editing() {
            match code {
                KeyCode::Backspace => {
                    field.text.pop();
                    return Action::None;
                }
                KeyCode::Char(c) => {
                    field.insert(c);
                    return Action::None;
                }
                _ => {}
            }
        }
        match &mut self.mode {
            Mode::List => self.list_key(code),
            Mode::Key { id, field, .. } if code == KeyCode::Enter => {
                Action::Change(Change::SetKey {
                    id: id.clone(),
                    key: field.text.clone(),
                })
            }
            Mode::Form(form) => form.key(code),
            Mode::Key { .. } | Mode::Confirm { .. } => Action::None,
        }
    }

    fn list_key(&mut self, code: KeyCode) -> Action {
        let rows = self.entries.len() + 2;
        let here = self.entries.get(self.cursor).cloned();
        let plan = |e: &Entry| e.source == Source::SignedIn;
        match (code, here) {
            (KeyCode::Down | KeyCode::Tab, _) => self.cursor = (self.cursor + 1) % rows,
            (KeyCode::Up | KeyCode::BackTab, _) => self.cursor = (self.cursor + rows - 1) % rows,
            (KeyCode::Char('l'), _) => return Action::SignIn,
            (KeyCode::Enter, None) if self.cursor > self.entries.len() => return Action::SignIn,
            (KeyCode::Char('a') | KeyCode::Enter, None) | (KeyCode::Char('a'), Some(_)) => {
                self.mode = Mode::Form(Form::new());
            }
            (KeyCode::Enter, Some(e)) if plan(&e) => return Action::SignIn,
            (KeyCode::Enter | KeyCode::Char('k'), Some(e)) if e.source == Source::Keyless => {
                self.status = Some(Err(format!(
                    "{} checks no key, so there is none to set.",
                    e.label
                )));
            }
            (KeyCode::Enter | KeyCode::Char('k'), Some(e)) if !plan(&e) => {
                self.mode = Mode::Key {
                    id: e.id,
                    label: e.label,
                    field: Field {
                        masked: true,
                        ..Field::default()
                    },
                };
            }
            (KeyCode::Char('x'), Some(e)) if e.source == Source::Vault || e.shadowed.is_some() => {
                return Action::Change(Change::ForgetKey(e.id));
            }
            (KeyCode::Char('x'), Some(e)) => {
                self.status = Some(Err(match (e.source, e.env_var) {
                    (Source::Environment, Some(var)) => format!(
                        "{}'s key comes from {var} in your environment: unset it there.",
                        e.label
                    ),
                    _ => format!("{} has no saved key to clear.", e.label),
                }));
            }
            (KeyCode::Char('d'), Some(e)) if plan(&e) || e.withdrawable => {
                let (verb, then) = if plan(&e) {
                    ("sign out of", Action::SignOut(e.id))
                } else {
                    ("remove", Action::Change(Change::Remove(e.id)))
                };
                self.mode = Mode::Confirm {
                    ask: format!("{verb} {}? y/n", e.label),
                    then,
                };
            }
            (KeyCode::Char('d'), Some(e)) => {
                self.status = Some(Err(format!(
                    "{} is a built-in service: x clears its saved key, but it cannot be removed.",
                    e.label
                )));
            }
            _ => {}
        }
        Action::None
    }
}

impl ProvidersOverlay {
    pub(super) fn render(&self, f: &mut Frame, frame: Rect) {
        let lines = self.body_lines();
        let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let h = (2 + 2 * PAD_Y + rows).min(frame.height.max(3));
        let area = centered(OVERLAY_W.min(frame.width), h, frame);
        let hint = match &self.mode {
            Mode::List => {
                " ↑↓ move · k key · x clear · a add endpoint · d remove · l sign in · esc close "
            }
            Mode::Key { .. } => " ⏎ save · esc back ",
            Mode::Form(_) => {
                " tab next · ←→ protocol / no key · ⏎ next / save · ^S save · esc back "
            }
            Mode::Confirm { .. } => " y yes · any other key no ",
        };
        let inner = overlay_frame(f, area, " PROVIDERS ", hint);
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(OVERLAY_BG)),
            inner,
        );
    }

    fn body_lines(&self) -> Vec<Line<'static>> {
        let mut lines = self.table();
        lines.push(Line::default());
        match &self.mode {
            Mode::List => {}
            Mode::Key { id, label, field } => {
                lines.push(Line::from(vec![
                    Span::styled(format!("▸ key for {label}  "), HOT),
                    Span::styled(format!("{}▏", field.shown()), HOT),
                ]));
                if let Some(Entry {
                    source: Source::Environment,
                    env_var: Some(var),
                    ..
                }) = self.entries.iter().find(|e| &e.id == id)
                {
                    let note = format!(
                        "{var} is set in your environment and takes priority; \
                         a key saved here is used when it isn't."
                    );
                    line::push_wrapped(&mut lines, &note, BODY_W - 2, |chunk, _| {
                        Line::styled(format!("  {chunk}"), DIM)
                    });
                }
            }
            Mode::Form(form) => lines.extend(form.fields.iter().enumerate().map(|(i, field)| {
                let on = i == form.focus;
                let value = match (on, i) {
                    (true, KEY) if form.keyless => "‹ none: the server checks no key ›".into(),
                    (false, KEY) if form.keyless => "none: the server checks no key".into(),
                    (true, PROTOCOL) => format!("‹ {} ›", field.text),
                    (true, _) => format!("{}▏", field.shown()),
                    _ => field.shown(),
                };
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{}{}",
                            if on { "▸ " } else { "  " },
                            FORM_LABEL.left(FORM[i])
                        ),
                        if on { HOT } else { DIM },
                    ),
                    Span::styled(value, if on { HOT } else { Style::default() }),
                ])
            })),
            Mode::Confirm { ask, .. } => lines.push(Line::styled(
                format!("  {ask}"),
                Style::new().fg(ORANGE).add_modifier(Modifier::BOLD),
            )),
        }
        if let Some(status) = &self.status {
            let (text, ink) = match status {
                Ok(text) => (text, LIME),
                Err(text) => (text, RED),
            };
            line::push_wrapped(&mut lines, text, BODY_W - 2, |chunk, _| {
                Line::styled(format!("  {chunk}"), Style::new().fg(ink))
            });
        }
        lines.push(Line::default());
        lines.push(Line::styled(format!("  vault: {}", self.vault), DIM));
        lines
    }

    fn table(&self) -> Vec<Line<'static>> {
        let mut rows: Vec<(&str, Color, [String; 3], String)> = self
            .entries
            .iter()
            .map(|e| {
                let (glyph, ink) = match e.source {
                    Source::SignedIn => ("◆", CYAN),
                    Source::Environment => ("●", PURPLE),
                    Source::Vault | Source::Keyless => ("●", LIME),
                    Source::None => ("○", SLATE),
                };
                let kind = match (&e.endpoint, &e.protocol) {
                    _ if e.source == Source::SignedIn => "plan login".to_string(),
                    (Some(address), Some(protocol)) => format!("{address} [{protocol}]"),
                    (Some(address), None) => address.clone(),
                    _ => "built-in".to_string(),
                };
                let key = match (&e.hint, e.source) {
                    (Some(hint), _) => format!("••••{hint}"),
                    (None, Source::None) => "— none —".to_string(),
                    (None, _) => "—".to_string(),
                };
                let state = match (e.source, &e.env_var) {
                    (Source::Environment, Some(var)) => match &e.shadowed {
                        Some(saved) => format!("from {var} (saved ••••{saved} beneath)"),
                        None => format!("from {var}"),
                    },
                    (Source::Vault, _) => "saved".to_string(),
                    (Source::SignedIn, _) => "signed in".to_string(),
                    (Source::Keyless, _) => "no key needed".to_string(),
                    (_, Some(var)) => format!("set {var} or press k"),
                    (_, None) => "press k to set a key".to_string(),
                };
                (glyph, ink, [e.label.clone(), kind, key], state)
            })
            .collect();
        let widths: [Col; 3] = std::array::from_fn(|c| {
            rows.iter()
                .fold(Col::wide(0).seeing(HEADS[c]), |w, (_, _, cells, _)| {
                    w.seeing(&cells[c])
                })
        });
        rows.extend(["+ add endpoint…", "+ sign in with ChatGPT"].map(|label| {
            (
                "○",
                SLATE,
                [label.to_string(), String::new(), String::new()],
                String::new(),
            )
        }));
        let head = HEADS
            .iter()
            .zip(widths)
            .fold(String::new(), |s, (head, w)| s + &w.left(head) + "  ");
        std::iter::once(Line::styled(format!("    {head}STATE"), DIM))
            .chain(
                rows.into_iter()
                    .enumerate()
                    .map(|(i, (glyph, ink, cells, state))| {
                        let on = i == self.cursor;
                        let text = if on { HOT } else { DIM };
                        let mut spans = vec![
                            Span::styled(if on { "▸ " } else { "  " }, HOT),
                            Span::styled(format!("{glyph} "), Style::new().fg(ink)),
                        ];
                        spans.extend(cells.iter().zip(widths).enumerate().map(|(c, (cell, w))| {
                            let style = if c == 0 && !on {
                                Style::default()
                            } else {
                                text
                            };
                            Span::styled(format!("{}  ", w.left(cell)), style)
                        }));
                        spans.push(Span::styled(state, text));
                        Line::from(spans)
                    }),
            )
            .collect()
    }
}

pub(super) fn providers(tui: &mut Tui, ctx: &CommandCtx<'_>) {
    let Some(holdings) = ctx.bureau.holdings() else {
        let root = tui.app.tabs.root();
        tui.app.push_error(
            root,
            "this session replays a scripted provider and keeps no accounts",
        );
        return;
    };
    let wallet = Wallet::exarch();
    loop {
        let table = entries(&holdings.store.lock_ignore_poison());
        let vault = wallet.keychain.vault().to_string();
        tui.app.overlay = Some(Overlay::Providers(ProvidersOverlay::new(table, vault)));
        let sign_in = drive(tui, ctx, holdings, &wallet);
        tui.app.overlay = None;
        if !sign_in {
            return;
        }
        login::login(tui, ctx);
    }
}

/// Poll keys and apply each [`Action`] until the overlay closes; `true` when
/// it closed to sign in, which the caller runs before reopening.
fn drive(tui: &mut Tui, ctx: &CommandCtx<'_>, holdings: &Holdings, wallet: &Wallet) -> bool {
    let (revoked, revocations) = mpsc::channel::<(String, Result<(), String>)>();
    loop {
        let tick = overlay_tick(tui);
        let Some(overlay) = tui.app.providers_mut() else {
            return false;
        };
        while let Ok((label, result)) = revocations.try_recv() {
            overlay.status = Some(match result {
                Ok(()) => Ok(format!("signed out of {label}; OpenAI revoked the token")),
                Err(e) => Err(format!(
                    "signed out of {label} here, but OpenAI did not revoke the token: {e}"
                )),
            });
        }
        let action = match tick {
            OverlayTick::TerminalLost => return false,
            OverlayTick::Cancel => {
                if overlay.cancel() {
                    return false;
                }
                continue;
            }
            OverlayTick::Idle => continue,
            OverlayTick::Paste(text) => {
                overlay.paste(&text);
                continue;
            }
            OverlayTick::Save => overlay.save(),
            OverlayTick::Key(code) => overlay.key(code),
        };
        let (done, id, result) = match action {
            Action::None => continue,
            Action::SignIn => return true,
            Action::SignOut(id) => {
                let result = ctx.bureau.sign_out(&id).map(|token| {
                    let (label, revoked) = (overlay.label(&id), revoked.clone());
                    // The local sign-out has happened: a lost revocation is best-effort.
                    thread::spawn(move || {
                        drop(revoked.send((label, oauth::revoke_blocking(&token))));
                    });
                });
                ("signed out of", id, result)
            }
            Action::Change(change) => change.apply(wallet, holdings),
        };
        let label = overlay.label(&id);
        let fresh = entries(&holdings.store.lock_ignore_poison());
        let now = fresh.iter().find(|e| e.id == id);
        let lost = result.is_ok() && now.is_none_or(|e| e.source == Source::None);
        let outranked = now
            .filter(|e| e.source == Source::Environment)
            .and_then(|e| e.env_var.clone());
        let outcome = result.map(|()| match outranked {
            Some(var) => format!("{done} {label}; {var} from your environment stays in force"),
            None => format!("{done} {label}"),
        });
        overlay.settled(outcome, fresh);
        let tabs = &tui.app.tabs;
        let active = tabs
            .ids()
            .into_iter()
            .filter_map(|tab| tabs.agent(tab))
            .any(|agent| agent.current_provider().account().id.as_str() == id);
        if lost && active {
            let text = format!(
                "[{done} {label}; the running model keeps its connection until you /model]"
            );
            if let Err(error) = ctx
                .recorder
                .emit(crate::record::Forensic::SystemNote { text })
            {
                ctx.recorder.report_fault(&error);
            }
        }
    }
}
