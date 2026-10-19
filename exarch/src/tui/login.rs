//! The `/login` overlay — "Sign in with `ChatGPT`" from inside a running
//! session.
//!
//! [`LoginOverlay`] is display and input only; the flow runs on a background
//! thread over [`oauth::login_flow`], reporting staged [`LoginPhase`]s as
//! [`Job`]s for the UI loop to apply — the same component/orchestration split
//! `model_picker.rs` makes.

use super::app::{App, CommandCtx, Input, Job, Overlay};
use super::overlay::{self, Anchor};
use super::palette::{CYAN, Col, RED, SLATE};
use super::providers;
use super::text;
use crate::provider::identity;
use crate::provider::oauth::{self, LoginPhase, OAuthToken, SignIn};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Wide enough for an id-qualified account label beside its blurb.
const BODY_W: u16 = 62;
/// The indent every body row shares.
const INDENT: &str = "  ";
/// The column every row's value starts after, so that no value is placed by
/// a hand-counted literal.  Two wider than the longest label.
const LABEL: Col = Col::wide(8);

/// Success closes the overlay, so there is no resting "done" mode.
enum Mode {
    Choosing,
    /// `None` until the first phase report lands, an instant after the thread
    /// spawns; it renders as every station pending. No key is live here — the
    /// cancel chord is the only way out.
    Running(Option<LoginPhase>),
    Failed(String),
}

/// One row of the selector: a sign-in and how the row names it.
struct Choice {
    sign_in: SignIn,
    label: String,
    blurb: &'static str,
}

pub(super) struct LoginOverlay {
    choices: Vec<Choice>,
    cursor: usize,
    mode: Mode,
    /// Whether this phase's value has gone to the terminal's clipboard.
    /// OSC 52 is never acknowledged, so this records what exarch sent, not
    /// what the terminal on the other end of the connection did with it.
    yanked: bool,
    /// The cancel flag of the flow in flight, if one is.
    flow: Option<Arc<AtomicBool>>,
    /// Whether closing returns to `/providers`, which opened this.
    back: bool,
}

impl LoginOverlay {
    /// A new registration first, then each signed-in account to sign in again.
    pub(super) fn new(back: bool) -> Self {
        let tokens = oauth::load_all();
        let accounts: Vec<_> = tokens.iter().map(oauth::to_account).collect();
        let renewals = tokens
            .into_iter()
            .zip(&accounts)
            .map(|(token, account)| Choice {
                label: identity::label(account, &accounts),
                sign_in: SignIn::Reauthorize(token),
                blurb: "sign in again",
            });
        let choices = std::iter::once(Choice {
            sign_in: SignIn::Register,
            label: "new account".to_string(),
            blurb: "register exarch with ChatGPT",
        })
        .chain(renewals)
        .collect();
        Self {
            choices,
            cursor: 0,
            mode: Mode::Choosing,
            yanked: false,
            flow: None,
            back,
        }
    }

    fn set_phase(&mut self, phase: LoginPhase) {
        self.mode = Mode::Running(Some(phase));
        self.yanked = false;
    }

    fn set_failed(&mut self, reason: String) {
        self.mode = Mode::Failed(reason);
    }
}

/// What a key press resolved to. No cancel variant: [`Input`] resolves the
/// cancel chord before a key ever reaches [`LoginOverlay::key`].
pub(super) enum LoginAction {
    None,
    Start(SignIn),
    /// Send this phase's one transcribable value to the host clipboard.
    Yank(String),
}

impl LoginOverlay {
    /// Handle one key press, mirroring `Picker::key` in `picker.rs`; movement
    /// wraps.
    pub(super) fn key(&mut self, code: KeyCode) -> LoginAction {
        let n = self.choices.len();
        match &self.mode {
            Mode::Choosing => match code {
                KeyCode::Enter => {
                    let sign_in = self.choices[self.cursor].sign_in.clone();
                    self.mode = Mode::Running(None);
                    LoginAction::Start(sign_in)
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.cursor = (self.cursor + 1) % n;
                    LoginAction::None
                }
                KeyCode::Up => {
                    self.cursor = (self.cursor + n - 1) % n;
                    LoginAction::None
                }
                _ => LoginAction::None,
            },
            Mode::Running(Some(phase)) if code == KeyCode::Char('y') => match phase {
                LoginPhase::AwaitingBrowser { url } => LoginAction::Yank(url.clone()),
                LoginPhase::ExchangingCode => LoginAction::None,
            },
            Mode::Running(_) => LoginAction::None,
            Mode::Failed(_) => match code {
                KeyCode::Enter => {
                    self.mode = Mode::Choosing;
                    LoginAction::None
                }
                _ => LoginAction::None,
            },
        }
    }
}

/// One station's state on the phase track. Borrows glyphs from the effort
/// ladder's `GLYPHS` (`picker.rs`), but three-valued: never a ramp, never a tick.
#[derive(Clone, Copy)]
enum Station {
    Pending,
    Active,
    Complete,
}

impl Station {
    fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "·",
            Self::Active => "▆",
            Self::Complete => "█",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Pending | Self::Complete => {
                Style::default().fg(SLATE).add_modifier(Modifier::DIM)
            }
            Self::Active => Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        }
    }

    fn span(self) -> Span<'static> {
        Span::styled(self.glyph(), self.style())
    }
}

impl LoginOverlay {
    /// Draw the overlay centred on `frame`.
    pub(super) fn render(&self, f: &mut Frame, frame: Rect) {
        let width = overlay::body_width(BODY_W, frame);
        let lines = self.body_lines(usize::from(width));

        let hint = match &self.mode {
            Mode::Choosing => " ↑↓ account · enter start · esc cancel ",
            Mode::Failed(_) => " enter retry · esc close ",
            Mode::Running(_) if self.yanked => " copied · esc cancel ",
            Mode::Running(Some(LoginPhase::AwaitingBrowser { .. })) => " y copy · esc cancel ",
            Mode::Running(_) => " esc cancel ",
        };
        let size = (width, lines.len());
        let body = overlay::draw(f, frame, Anchor::Centre, size, " SIGN IN ", hint);
        f.render_widget(Paragraph::new(lines), body);
    }

    fn body_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines: Vec<_> = self
            .choices
            .iter()
            .enumerate()
            .map(|(i, choice)| self.choice_line(i, choice))
            .collect();
        lines.push(Line::default());
        lines.push(self.phase_line());
        lines.extend(self.detail_lines(width));
        lines
    }

    /// One row of the selector; the `account` column header sits on the first.
    fn choice_line(&self, index: usize, choice: &Choice) -> Line<'static> {
        let active = self.cursor == index;
        let name_style = if active {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            dim()
        };
        let lead = if index == 0 { "account" } else { "" };
        let marker = if active { "▸ " } else { "  " };
        Line::from(vec![
            Span::styled(format!("{INDENT}{}{marker}", LABEL.left(lead)), dim()),
            Span::styled(choice.label.clone(), name_style),
            Span::styled(format!("  {}", choice.blurb), dim()),
        ])
    }

    /// The phase track: `step   █ start   ▆ sign in   · exchange`. Stations
    /// hold their positions and the mapping from [`Mode`] is static — nothing
    /// ticks, nothing sweeps, no elapsed counter.
    fn phase_line(&self) -> Line<'static> {
        let (start, sign_in, exchange) = self.stations();
        Line::from(vec![
            Span::styled(format!("{INDENT}{}", LABEL.left("step")), dim()),
            start.span(),
            Span::styled(" start   ", start.style()),
            sign_in.span(),
            Span::styled(" sign in   ", sign_in.style()),
            exchange.span(),
            Span::styled(" exchange", exchange.style()),
        ])
    }

    fn stations(&self) -> (Station, Station, Station) {
        match &self.mode {
            Mode::Choosing | Mode::Running(None) => {
                (Station::Pending, Station::Pending, Station::Pending)
            }
            Mode::Running(Some(LoginPhase::ExchangingCode)) => {
                (Station::Complete, Station::Complete, Station::Active)
            }
            Mode::Running(Some(_)) => (Station::Complete, Station::Active, Station::Pending),
            // A failure carries its reason but not its stage, so all-pending is
            // the honest reading rather than a fabricated "got this far".
            Mode::Failed(_) => (Station::Pending, Station::Pending, Station::Pending),
        }
    }

    /// The phase-specific rows below the track. The alt screen swallows stderr,
    /// so what [`LoginPhase::stderr_line`] gives the CLI must appear here or
    /// nowhere — the sign-in URL above all.
    fn detail_lines(&self, width: usize) -> Vec<Line<'static>> {
        match &self.mode {
            Mode::Choosing | Mode::Running(None | Some(LoginPhase::ExchangingCode)) => Vec::new(),
            Mode::Running(Some(LoginPhase::AwaitingBrowser { url })) => {
                field("open", url, dim(), width)
            }
            Mode::Failed(reason) => field("error", reason, Style::default().fg(RED), width),
        }
    }
}

/// The overlay's recessive ink: labels, blurbs, and every value but the one
/// the user has to carry to another machine.
fn dim() -> Style {
    Style::default().fg(SLATE).add_modifier(Modifier::DIM)
}

/// A labelled row whose value wraps on under the value column, through the
/// shared display-width-aware [`text::hang`].  Wrapping is what stops
/// a long value clipping at the bezel and reading as a shorter one.
fn field(label: &str, text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let head = [Span::styled(
        format!("{INDENT}{}", LABEL.left(label)),
        dim(),
    )];
    text::hang(&head, vec![Span::styled(text.to_string(), style)], width)
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

pub(super) fn login(app: &mut App) {
    app.open(Overlay::Login(LoginOverlay::new(false)));
}

/// Cancelling trips the flow's flag — `login_flow`'s wait loops poll it every
/// 100 ms, freeing the loopback listener — and returns at once; the orphan's
/// last jobs see the flag and do nothing.
pub(super) fn input(
    l: &mut LoginOverlay,
    input: &Input,
    app: &mut App,
    ctx: &CommandCtx<'_>,
) -> bool {
    match input {
        Input::Cancel => {
            if let Some(flow) = &l.flow {
                // `Relaxed` suffices: the flag publishes no data of its own,
                // and the phase and result payloads ride the jobs.
                flow.store(true, Ordering::Relaxed);
            }
            if l.back {
                providers::providers(app, ctx);
            }
            false
        }
        Input::Key(code) => {
            match l.key(*code) {
                LoginAction::Start(sign_in) => {
                    let cancel = Arc::new(AtomicBool::new(false));
                    l.flow = Some(Arc::clone(&cancel));
                    let jobs = app.jobs.clone();
                    std::thread::spawn(move || {
                        let (phase_jobs, flag) = (jobs.clone(), Arc::clone(&cancel));
                        let result = oauth::login_flow(
                            &sign_in,
                            move |phase| {
                                let flag = Arc::clone(&flag);
                                let job: Job = Box::new(move |app, _| {
                                    if !flag.load(Ordering::Relaxed)
                                        && let Some(Overlay::Login(l)) = &mut app.overlay
                                    {
                                        l.set_phase(phase);
                                    }
                                });
                                let _ = phase_jobs.send(job);
                            },
                            &cancel,
                        );
                        let _ = jobs.send(Box::new(move |app, ctx| {
                            if cancel.load(Ordering::Relaxed) {
                                return;
                            }
                            match result {
                                Ok((token, replaced)) => {
                                    let back =
                                        matches!(app.overlay.take(), Some(Overlay::Login(l)) if l.back);
                                    apply_login(app, ctx, &token, replaced);
                                    if back {
                                        providers::providers(app, ctx);
                                    }
                                }
                                Err(e) => {
                                    if let Some(Overlay::Login(l)) = &mut app.overlay {
                                        l.set_failed(e);
                                        l.flow = None;
                                    }
                                }
                            }
                        }));
                    });
                }
                // Only the write is ours to judge: over OSC 52 no reply comes, so
                // whether the terminal at the far end honoured it is unknowable.
                LoginAction::Yank(text) => {
                    if app.clipboard.copy(&text, true).is_ok() {
                        l.yanked = true;
                    }
                }
                LoginAction::None => {}
            }
            true
        }
        Input::Save | Input::Paste(_) => true,
    }
}

/// Make the token `login_flow` already persisted live in this session's
/// credential store and catalog. No provider swap: an account names no model
/// of its own, so the user picks one through `/model`; and a re-login upserts
/// the very cell the focused tab already reads through.
fn apply_login(app: &mut App, ctx: &CommandCtx<'_>, token: &OAuthToken, replaced: bool) {
    let admitted = ctx.bureau.admit(token);
    let (id, label) = match admitted {
        Ok(admitted) => admitted,
        Err(e) => {
            app.push_error(app.tabs.focused(), &e);
            return;
        }
    };
    let already_active = app
        .tabs
        .focused_agent()
        .is_some_and(|agent| agent.current_provider().account().id == id);
    let action = if replaced {
        "Updated the login for"
    } else {
        "Signed in to"
    };
    let next = if already_active {
        ""
    } else {
        "; run /model to use it"
    };
    let text = format!("[{action} ChatGPT account {label}{next}]");
    app.tabs
        .focused_recorder()
        .emit_or_report(crate::record::Forensic::SystemNote { text });
}
