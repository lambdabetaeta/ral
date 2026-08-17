//! The `/login` overlay — "Sign in with `ChatGPT`" from inside a running
//! session.
//!
//! [`LoginOverlay`] is display and input only; the flow runs on a background
//! thread over [`oauth::login_flow`], reporting staged [`LoginPhase`]s over an
//! `mpsc` channel while [`drive_login`] polls keys and redraws — the same
//! component/orchestration split `model_picker.rs` makes.

use super::app::Overlay;
use super::line;
use super::palette::{BANNER_GOLD, CYAN, OVERLAY_BG, RED, SLATE};
use super::picker::{PAD_X, PAD_Y, centered, overlay_frame};
use super::terminal::osc52_copy;
use super::tui_loop::{CommandCtx, OverlayTick, Tui, overlay_tick};
use crate::provider::identity;
use crate::provider::oauth::{self, LoginMethod, LoginPhase, OAuthToken};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

const OVERLAY_W: u16 = 56;
/// Wrap width for the body rows: [`OVERLAY_W`] less bezel and padding.
const BODY_W: usize = OVERLAY_W as usize - 2 - 2 * PAD_X as usize;
/// The indent every body row shares.
const INDENT: &str = "  ";
/// The column every row's value starts after, so that no value is placed by
/// a hand-counted literal.  Two wider than the longest label.
const LABEL_W: usize = 8;
/// What a labelled row leaves for its value.
const VALUE_W: usize = BODY_W - INDENT.len() - LABEL_W;

/// Success closes the overlay, so there is no resting "done" mode.
enum Mode {
    Choosing,
    /// `None` until the first phase report lands, an instant after the thread
    /// spawns; it renders as every station pending. No key is live here — the
    /// driver's cancel chord is the only way out.
    Running(Option<LoginPhase>),
    Failed(String),
}

pub(super) struct LoginOverlay {
    method: LoginMethod,
    mode: Mode,
    /// Whether this phase's value has gone to the terminal's clipboard.
    /// OSC 52 is never acknowledged, so this records what exarch sent, not
    /// what the terminal on the other end of the connection did with it.
    yanked: bool,
}

impl LoginOverlay {
    fn new() -> Self {
        Self {
            method: LoginMethod::Browser,
            mode: Mode::Choosing,
            yanked: false,
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

/// What a key press resolved to. No cancel variant: `overlay_tick` resolves
/// the cancel chord before a key ever reaches [`LoginOverlay::key`].
pub(super) enum LoginAction {
    None,
    Start(LoginMethod),
    /// Send this phase's one transcribable value to the host clipboard.
    Yank(String),
}

impl LoginOverlay {
    /// Handle one key press, mirroring `Picker::key` in `picker.rs`. With two
    /// methods, every movement key simply toggles.
    pub(super) fn key(&mut self, code: KeyCode) -> LoginAction {
        match &self.mode {
            Mode::Choosing => match code {
                KeyCode::Enter => {
                    let method = self.method;
                    self.mode = Mode::Running(None);
                    LoginAction::Start(method)
                }
                KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    self.method = match self.method {
                        LoginMethod::Browser => LoginMethod::Device,
                        LoginMethod::Device => LoginMethod::Browser,
                    };
                    LoginAction::None
                }
                _ => LoginAction::None,
            },
            Mode::Running(Some(phase)) if code == KeyCode::Char('y') => match phase {
                LoginPhase::AwaitingBrowser { url } => LoginAction::Yank(url.clone()),
                LoginPhase::AwaitingDevice { user_code, .. } => {
                    LoginAction::Yank(user_code.clone())
                }
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
    /// Draw the overlay centred on `frame`, in the picker's own chrome — one
    /// shell, two overlays.
    pub(super) fn render(&self, f: &mut Frame, frame: Rect) {
        let (w, h) = self.desired_size(frame);
        let area = centered(w, h, frame);
        let plane = Style::default().bg(OVERLAY_BG);

        let hint = match &self.mode {
            Mode::Choosing => " ↑↓ method · ⏎ start · esc cancel ",
            Mode::Failed(_) => " ⏎ retry · esc close ",
            Mode::Running(_) if self.yanked => " copied · esc cancel ",
            Mode::Running(Some(
                LoginPhase::AwaitingBrowser { .. } | LoginPhase::AwaitingDevice { .. },
            )) => " y copy · esc cancel ",
            Mode::Running(_) => " esc cancel ",
        };
        let inner = overlay_frame(f, area, " SIGN IN ", hint);
        f.render_widget(Paragraph::new(self.body_lines()).style(plane), inner);
    }

    fn desired_size(&self, frame: Rect) -> (u16, u16) {
        let w = OVERLAY_W.min(frame.width);
        #[allow(clippy::cast_possible_truncation, reason = "a handful of body rows")]
        let body_rows = self.body_lines().len() as u16;
        let h = 2 + 2 * PAD_Y + body_rows;
        (w, h.min(frame.height.max(3)))
    }

    fn body_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            self.method_line(LoginMethod::Browser, "browser", "open a browser tab", true),
            self.method_line(
                LoginMethod::Device,
                "device",
                "type a code elsewhere",
                false,
            ),
            Line::default(),
            self.phase_line(),
        ];
        lines.extend(self.detail_lines());
        lines
    }

    /// One row of the method selector. `show_field_label` prints the `method`
    /// column header on the first row only.
    fn method_line(
        &self,
        method: LoginMethod,
        label: &str,
        blurb: &str,
        show_field_label: bool,
    ) -> Line<'static> {
        let active = self.method == method;
        let name_style = if active {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            dim()
        };
        let lead = if show_field_label { "method" } else { "" };
        let marker = if active { "▸ " } else { "  " };
        Line::from(vec![
            Span::styled(format!("{INDENT}{lead:<LABEL_W$}{marker}"), dim()),
            Span::styled(format!("{label:<7}"), name_style),
            Span::styled(format!("  {blurb}"), dim()),
        ])
    }

    /// The phase track: `step   █ start   ▆ sign in   · exchange`. Stations
    /// hold their positions and the mapping from [`Mode`] is static — nothing
    /// ticks, nothing sweeps, no elapsed counter.
    fn phase_line(&self) -> Line<'static> {
        let (start, sign_in, exchange) = self.stations();
        Line::from(vec![
            Span::styled(format!("{INDENT}{:<LABEL_W$}", "step"), dim()),
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
    fn detail_lines(&self) -> Vec<Line<'static>> {
        match &self.mode {
            Mode::Choosing | Mode::Running(None | Some(LoginPhase::ExchangingCode)) => Vec::new(),
            Mode::Running(Some(LoginPhase::AwaitingBrowser { url })) => field("open", url, dim()),
            Mode::Running(Some(LoginPhase::AwaitingDevice {
                user_code,
                url,
                expires_in,
            })) => {
                let mut lines = vec![Line::from(vec![
                    Span::styled(format!("{INDENT}{:<LABEL_W$}", "code"), dim()),
                    Span::styled(
                        user_code.clone(),
                        Style::default()
                            .fg(BANNER_GOLD)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("   expires in {expires_in}"), dim()),
                ])];
                lines.extend(field("open", url, dim()));
                lines
            }
            Mode::Failed(reason) => field("error", reason, Style::default().fg(RED)),
        }
    }
}

/// The overlay's recessive ink: labels, blurbs, and every value but the one
/// the user has to carry to another machine.
fn dim() -> Style {
    Style::default().fg(SLATE).add_modifier(Modifier::DIM)
}

/// A labelled row whose value wraps on under the value column, through the
/// shared display-width-aware [`line::push_wrapped`].  Wrapping is what stops
/// a long value clipping at the bezel and reading as a shorter one.
fn field(label: &str, text: &str, style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    line::push_wrapped(&mut lines, text, VALUE_W, |chunk, first| {
        let lead = if first { label } else { "" };
        Line::from(vec![
            Span::styled(format!("{INDENT}{lead:<LABEL_W$}"), dim()),
            Span::styled(chunk, style),
        ])
    });
    lines
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Message from the flow thread to the overlay loop.
enum LoginMsg {
    Phase(LoginPhase),
    /// The persisted token and whether it replaced an account, or the failure.
    Done(Result<(OAuthToken, bool), String>),
}

pub(super) fn login(tui: &mut Tui, ctx: &mut CommandCtx<'_>) {
    tui.app.overlay = Some(Overlay::Login(LoginOverlay::new()));
    let outcome = drive_login(tui);
    tui.app.overlay = None;
    if let Some((token, replaced)) = outcome {
        apply_login(tui, ctx, &token, replaced);
    }
}

/// A flow thread in flight. Cancelling trips the flag — `login_flow`'s wait
/// loops poll it every 100 ms, freeing the loopback listener — and returns at
/// once, leaving the orphan's last message to land in a dropped `Receiver`.
struct FlowHandle {
    rx: mpsc::Receiver<LoginMsg>,
    cancel: Arc<AtomicBool>,
}

/// Poll keys and the flow's messages until the overlay resolves; `None` on
/// cancel or close. The flow's own thread keeps this loop off the network.
fn drive_login(tui: &mut Tui) -> Option<(OAuthToken, bool)> {
    let mut flow: Option<FlowHandle> = None;
    loop {
        let mut clear_flow = false;
        if let Some(handle) = flow.as_ref() {
            loop {
                match handle.rx.try_recv() {
                    Ok(LoginMsg::Phase(phase)) => {
                        if let Some(l) = tui.app.login_mut() {
                            l.set_phase(phase);
                        }
                    }
                    Ok(LoginMsg::Done(Ok(pair))) => return Some(pair),
                    Ok(LoginMsg::Done(Err(e))) => {
                        if let Some(l) = tui.app.login_mut() {
                            l.set_failed(e);
                        }
                        clear_flow = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        if clear_flow {
            flow = None;
        }

        match overlay_tick(tui) {
            OverlayTick::TerminalLost => return None,
            OverlayTick::Idle => {}
            OverlayTick::Cancel => {
                if let Some(handle) = &flow {
                    // `Relaxed` suffices: the flag publishes no data of its own,
                    // and the phase and result payloads ride the channel.
                    handle.cancel.store(true, Ordering::Relaxed);
                }
                return None;
            }
            OverlayTick::Key(code) => match tui.app.login_mut()?.key(code) {
                LoginAction::Start(method) => {
                    let (tx, rx) = mpsc::channel();
                    let cancel = Arc::new(AtomicBool::new(false));
                    let flag = Arc::clone(&cancel);
                    let phase_tx = tx.clone();
                    std::thread::spawn(move || {
                        let result = oauth::login_flow(
                            method,
                            move |p| {
                                let _ = phase_tx.send(LoginMsg::Phase(p));
                            },
                            &flag,
                        );
                        let _ = tx.send(LoginMsg::Done(result));
                    });
                    flow = Some(FlowHandle { rx, cancel });
                }
                // Only the write is ours to judge: OSC 52 draws no reply, so
                // whether the terminal at the far end honoured it is unknowable.
                LoginAction::Yank(text) => {
                    if osc52_copy(&text).is_ok()
                        && let Some(l) = tui.app.login_mut()
                    {
                        l.yanked = true;
                    }
                }
                LoginAction::None => {}
            },
        }
    }
}

/// Make the token `login_flow` already persisted live in this session's
/// credential store and catalog. No provider swap: a `ChatGPT` account has no
/// built-in default model, so the user picks one through `/model`; and a
/// re-login upserts the very cell the focused tab already reads through.
fn apply_login(tui: &Tui, ctx: &mut CommandCtx<'_>, token: &OAuthToken, replaced: bool) {
    let (account, credential) = ctx.store.add_oauth(token);
    let already_active = ctx
        .agents
        .provider(tui.app.tabs.focused())
        .is_some_and(|provider| provider.current().account().id == account.id);
    let action = if replaced {
        "Updated the login for"
    } else {
        "Signed in to"
    };
    let next = if already_active {
        ""
    } else {
        " — run /model to use it"
    };
    // The store's name for it, which says which account when two share an email.
    let text = format!(
        "[{action} ChatGPT account {}{next}]",
        identity::label(&account, &ctx.store.available())
    );
    ctx.catalog.add_credential(account, credential);
    if let Err(error) = ctx
        .recorder
        .emit(crate::record::Forensic::SystemNote { text })
    {
        ctx.recorder.report_fault(&error);
    }
}
