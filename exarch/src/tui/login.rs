//! The `/login` overlay — "Sign in with `ChatGPT`" driven from inside a running
//! session, modeled on the `/model` picker (`picker.rs` / `model_picker.rs`).
//!
//! [`LoginOverlay`] is a pure display+input component (state + key + render,
//! no I/O), the picker's own discipline in miniature: it is a fraction of the
//! picker's size (no fuzzy list, no catalog, no tuning tracks), so it earns
//! no component/orchestration file split. The orchestration below it —
//! [`login`], [`drive_login`], [`apply_login`] — mirrors `pick_model` /
//! `drive_picker` / `apply_model_switch` (`model_picker.rs`): the flow itself
//! runs on a background thread over [`crate::provider::oauth::login_flow`],
//! reporting staged [`crate::provider::oauth::LoginPhase`]s back over an
//! `mpsc` channel while this loop polls keys and redraws.
//!
//! The overlay renders progress as a **fixed-position three-station track**
//! (`step`), never a spinner or a live countdown — the Bertin-compliant
//! discrete-phase encoding the effort ladder (`picker.rs`) already
//! establishes: only a station's glyph/brightness changes between phases.

use super::app::Overlay;
use super::line;
use super::palette::{BANNER_GOLD, CYAN, OVERLAY_BG, RED, SLATE};
use super::picker::{PAD_X, PAD_Y, bezel_shell, centered};
use super::tui_loop::{CommandCtx, OverlayTick, Tui, overlay_tick};
use crate::bus::Kind;
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

/// The overlay's fixed content width; clamped to the frame when narrower.
const OVERLAY_W: u16 = 56;
/// The wrap width for the detail rows (the manual-open URL, the failure
/// reason): the content width less the bezel border and its padding.
const BODY_W: usize = OVERLAY_W as usize - 2 - 2 * PAD_X as usize;

/// The overlay's mode: choosing a method, watching the running flow, or
/// showing a terminal failure (success closes the overlay, so it has no
/// resting "done" mode).
enum Mode {
    /// Method selector has the keyboard; Enter starts the flow.
    Choosing,
    /// The flow is running on its background thread. `None` until the first
    /// phase report lands (an instant after the thread spawns, rendering as
    /// every station pending); `Some` holds the last staged report. No key is
    /// live — the driver's cancel chord (Esc, Ctrl-C, Ctrl-D) is the only way
    /// out.
    Running(Option<LoginPhase>),
    /// The flow failed; the reason is shown. Enter returns to `Choosing`
    /// (retry); the driver's cancel chord closes.
    Failed(String),
}

pub(super) struct LoginOverlay {
    method: LoginMethod,
    mode: Mode,
}

impl LoginOverlay {
    fn new() -> Self {
        Self {
            method: LoginMethod::Browser,
            mode: Mode::Choosing,
        }
    }

    fn set_phase(&mut self, phase: LoginPhase) {
        self.mode = Mode::Running(Some(phase));
    }

    fn set_failed(&mut self, reason: String) {
        self.mode = Mode::Failed(reason);
    }
}

/// What a key press resolved to. [`drive_login`] acts on the non-`None`
/// outcome by spawning the flow thread; cancellation (Esc, Ctrl-C, Ctrl-D) is
/// resolved by the driver before a key ever reaches [`LoginOverlay::key`], so
/// it never appears here.
pub(super) enum LoginAction {
    /// Keep the overlay open; redraw.
    None,
    /// Enter in `Choosing`: start the flow with the selected method.
    Start(LoginMethod),
}

impl LoginOverlay {
    /// Handle one key press. Mirrors `Picker::key` (`picker.rs`). `Choosing`:
    /// any movement key toggles the two-way method selector, Enter starts.
    /// `Running`: nothing is live — there is nothing to edit mid-flow.
    /// `Failed`: Enter retries (back to `Choosing`).
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

/// One station's rendered state on the fixed three-station phase track — the
/// effort ladder's own glyph vocabulary (`picker.rs`'s `LADDER`), applied
/// discretely rather than as a ramp: pending, active, or complete, never a
/// value in between and never animated.
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
    /// Draw the floating overlay over the centre of `frame`, reusing the
    /// picker's own chrome (`centered`, `bezel_shell`) rather than copying it
    /// — one overlay shell, two overlays. Mirrors `Picker::render`
    /// (`picker.rs`).
    pub(super) fn render(&self, f: &mut Frame, frame: Rect) {
        let (w, h) = self.desired_size(frame);
        let area = centered(w, h, frame);
        let plane = Style::default().bg(OVERLAY_BG);

        let hint = match self.mode {
            Mode::Choosing => " ↑↓ method · ⏎ start · esc cancel ",
            Mode::Running(_) => " esc cancel ",
            Mode::Failed(_) => " ⏎ retry · esc close ",
        };
        let inner = bezel_shell(f, area, plane, " SIGN IN ", hint);
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

    /// One row of the method selector — bright cyan for the active method,
    /// dim otherwise, the picker's own `field_label` focus treatment
    /// (`picker.rs`) in miniature. `show_field_label` prints the `method`
    /// column header on the first row only, blank on the second.
    fn method_line(
        &self,
        method: LoginMethod,
        label: &str,
        blurb: &str,
        show_field_label: bool,
    ) -> Line<'static> {
        let dim = Style::default().fg(SLATE).add_modifier(Modifier::DIM);
        let active = self.method == method;
        let name_style = if active {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        let field = if show_field_label {
            "method "
        } else {
            "       "
        };
        let marker = if active { "▸" } else { " " };
        Line::from(vec![
            Span::styled(format!("  {field} {marker} "), dim),
            Span::styled(format!("{label:<7}"), name_style),
            Span::styled(format!("  {blurb}"), dim),
        ])
    }

    /// The fixed-position three-station phase track: `step   █ start   ▆ sign
    /// in   · exchange`. The mapping from [`Mode`] is total and static —
    /// nothing ticks, nothing sweeps, no elapsed counter.
    fn phase_line(&self) -> Line<'static> {
        let (start, sign_in, exchange) = self.stations();
        Line::from(vec![
            Span::styled(
                "  step    ",
                Style::default().fg(SLATE).add_modifier(Modifier::DIM),
            ),
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
            // The failed attempt's exact stage is not tracked (only its
            // reason is) — showing every station pending is the honest
            // reading, not a fabricated "got this far".
            Mode::Failed(_) => (Station::Pending, Station::Pending, Station::Pending),
        }
    }

    /// The phase-specific detail row(s) below the track: the browser's
    /// manual-open fallback (the alt-screen replacement for
    /// `browser.rs`'s stderr fallback — it must be visible here or nowhere),
    /// the device code and its static expiry text, or the failure reason.
    fn detail_lines(&self) -> Vec<Line<'static>> {
        match &self.mode {
            Mode::Choosing | Mode::Running(None | Some(LoginPhase::ExchangingCode)) => Vec::new(),
            Mode::Running(Some(LoginPhase::AwaitingBrowser { opened: true, .. })) => {
                vec![dim_line("  complete the sign-in in your browser")]
            }
            Mode::Running(Some(LoginPhase::AwaitingBrowser { url, opened: false })) => {
                let mut lines = vec![dim_line("  open this URL to sign in:")];
                lines.extend(wrap(
                    url,
                    Style::default().fg(SLATE).add_modifier(Modifier::DIM),
                ));
                lines
            }
            Mode::Running(Some(LoginPhase::AwaitingDevice { user_code, url })) => vec![
                Line::from(vec![
                    Span::styled(
                        "  code    ",
                        Style::default().fg(SLATE).add_modifier(Modifier::DIM),
                    ),
                    Span::styled(
                        user_code.clone(),
                        Style::default()
                            .fg(BANNER_GOLD)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "   expires in 15 minutes",
                        Style::default().fg(SLATE).add_modifier(Modifier::DIM),
                    ),
                ]),
                dim_line(&format!("           {url}")),
            ],
            Mode::Failed(reason) => wrap(reason, Style::default().fg(RED)),
        }
    }
}

fn dim_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(SLATE).add_modifier(Modifier::DIM),
    ))
}

/// Word-wrap `text` to [`BODY_W`] columns (less its two-column indent),
/// styled uniformly and left-indented — used for the two detail texts long
/// enough to need it (a manual-open URL, a failure reason). Backed by
/// [`line::push_wrapped`], the shared display-width-aware wrap primitive
/// (`picker.rs`'s failed-provider notes use it for the same job).
fn wrap(text: &str, style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    line::push_wrapped(&mut lines, text, BODY_W.saturating_sub(2), |chunk, _| {
        Line::from(Span::styled(format!("  {chunk}"), style))
    });
    lines
}

// ---------------------------------------------------------------------------
// Orchestration — mirrors `pick_model` / `drive_picker` / `apply_model_switch`
// (`model_picker.rs`).
// ---------------------------------------------------------------------------

/// Message from the flow thread to the overlay loop.
enum LoginMsg {
    Phase(LoginPhase),
    /// The flow's outcome: the persisted token plus whether an existing
    /// account was replaced, or the failure text.
    Done(Result<(OAuthToken, bool), String>),
}

/// `/login` — open the sign-in overlay, drive it to completion or
/// cancellation, then commit a successful result.
pub(super) fn login(tui: &mut Tui, ctx: &mut CommandCtx<'_>) {
    tui.app.overlay = Some(Overlay::Login(LoginOverlay::new()));
    let outcome = drive_login(tui);
    tui.app.overlay = None;
    if let Some((token, replaced)) = outcome {
        apply_login(ctx, &token, replaced);
    }
}

/// A flow thread in flight: its message channel and its cancel flag. Esc
/// trips the flag (the wait loops inside `login_flow` poll it, freeing the
/// loopback listener within ~100 ms) and the overlay closes at once; the
/// orphaned thread's eventual message lands in a dropped `Receiver` and
/// vanishes.
struct FlowHandle {
    rx: mpsc::Receiver<LoginMsg>,
    cancel: Arc<AtomicBool>,
}

/// Poll keys and the running flow's messages until the overlay resolves.
/// Returns the persisted `(token, replaced)` on success, `None` on
/// cancel/close. Structured exactly as `drive_picker` (`model_picker.rs`):
/// the flow runs on a background thread reporting over an `mpsc` channel, so
/// this render/poll loop never blocks on the network.
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
                    handle.cancel.store(true, Ordering::Release);
                }
                return None;
            }
            OverlayTick::Key(code) => {
                if let LoginAction::Start(method) = tui.app.login_mut()?.key(code) {
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
            }
        }
    }
}

/// The commit step (mirrors `apply_model_switch`, `model_picker.rs`).
/// The flow has already persisted the token to disk (`save_one`, inside
/// `login_flow`); this makes it *live* in the running session — the
/// credential store and the model catalog's `LiveSource` — and records the
/// event. No provider swap: a `ChatGPT` account has no built-in default model
/// (`lib.rs`'s `resolve_initial_selection`), so the user is not
/// auto-switched — the note points at `/model`. A re-login for the
/// *currently selected* account still needs no swap: `add_oauth`'s upsert
/// refreshes the very cell the live provider already reads through.
fn apply_login(ctx: &mut CommandCtx<'_>, token: &OAuthToken, replaced: bool) {
    let id = ctx.store.add_oauth(token);
    let cred = ctx
        .store
        .get(&id)
        .cloned()
        .expect("add_oauth just inserted this id's credential");
    ctx.catalog.source_mut().add_credential(id, cred);
    ctx.emit.emit(Kind::SystemNote(format!(
        "[{} ChatGPT account {} — run /model to use it]",
        if replaced {
            "Updated the login for"
        } else {
            "Signed in to"
        },
        token.label(),
    )));
}
