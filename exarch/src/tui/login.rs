//! The `/login` overlay — plan sign-ins from inside a running session.
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
use crate::provider::identity::{self, Login};
use crate::provider::oauth::{self, LoginPhase, Method, OAuthToken, SignIn};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Wide enough for an id-qualified account label beside its blurb.
const BODY_W: u16 = 80;
/// The indent every body row shares.
const INDENT: &str = "  ";
/// The column every row's value starts after, so that no value is placed by
/// a hand-counted literal.  Two wider than the longest label.
const LABEL: Col = Col::wide(15);

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
    heading: String,
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
    /// Group services, then new sign-ins and sorted renewals for every method.
    pub(super) fn new(back: bool) -> Self {
        Self::with_tokens(&oauth::load_all(), back)
    }

    fn with_tokens(tokens: &[OAuthToken], back: bool) -> Self {
        let accounts: Vec<_> = tokens.iter().map(oauth::to_account).collect();
        let mut renewals: Vec<_> = tokens.iter().zip(&accounts).collect();
        renewals.sort_by(|(_, a), (_, b)| (&a.handle, &a.id).cmp(&(&b.handle, &b.id)));
        let mut choices = Vec::new();
        for login in Login::ALL {
            let heading = login.service().name.to_string();
            for &method in oauth::methods(login) {
                choices.push(Choice {
                    sign_in: SignIn::New(login, method),
                    heading: heading.clone(),
                    label: "new account".to_string(),
                    blurb: match method {
                        Method::Browser => "browser",
                        Method::Device => "device code",
                    },
                });
            }
            for (token, account) in renewals
                .iter()
                .copied()
                .filter(|(token, _)| token.login() == login)
            {
                for &method in oauth::methods(login) {
                    choices.push(Choice {
                        sign_in: SignIn::Renew(token.clone(), method),
                        heading: heading.clone(),
                        label: identity::label(account, &accounts),
                        blurb: match method {
                            Method::Browser => "sign in again · browser",
                            Method::Device => "sign in again · device code",
                        },
                    });
                }
            }
        }
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
    /// Draw the overlay centred on `frame`.
    pub(super) fn render(&self, f: &mut Frame, frame: Rect) {
        let width = overlay::body_width(BODY_W, frame);
        let lines = self.body_lines(usize::from(width), overlay::body_height(frame));
        let body = overlay::draw(
            f,
            frame,
            Anchor::Centre,
            (width, lines.len()),
            " SIGN IN ",
            self.hint(),
        );
        f.render_widget(Paragraph::new(lines), body);
    }

    fn hint(&self) -> &'static str {
        match &self.mode {
            Mode::Choosing => " ↑↓ account · enter start · esc cancel ",
            Mode::Failed(_) => " enter retry · esc close ",
            Mode::Running(Some(
                LoginPhase::AwaitingBrowser { .. } | LoginPhase::AwaitingDevice { .. },
            )) => {
                if self.yanked {
                    " copied · esc cancel "
                } else {
                    " y copy · esc cancel "
                }
            }
            Mode::Running(_) => " esc cancel ",
        }
    }

    fn body_lines(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        if height == 0 {
            return Vec::new();
        }
        if matches!(self.mode, Mode::Choosing) {
            return self.choosing_lines(height);
        }
        let (details, required) = self.detail_lines(width);
        let header_rows = height.saturating_sub(required).min(3);
        let mut lines = Vec::new();
        if header_rows > 0 {
            lines.push(self.choice_line(self.cursor, true));
        }
        if header_rows > 2 {
            lines.push(Line::default());
        }
        if header_rows > 1 {
            lines.push(self.phase_line());
        }
        lines.extend(details.into_iter().take(height.saturating_sub(lines.len())));
        lines
    }

    /// Headings share selector rows, including the first visible row when its
    /// group's original heading has scrolled away. Reserve one clipping row.
    fn choosing_lines(&self, height: usize) -> Vec<Line<'static>> {
        let clipped = self.choices.len() > height;
        let rows = height.saturating_sub(usize::from(clipped && height > 1));
        let start = self.cursor.saturating_sub(rows.saturating_sub(1));
        let end = (start + rows).min(self.choices.len());
        let mut lines: Vec<_> = (start..end)
            .map(|i| self.choice_line(i, i == start))
            .collect();
        if clipped && height > 1 {
            lines.push(Line::styled(
                format!(
                    "{INDENT}… {start} above · {} below",
                    self.choices.len() - end
                ),
                dim(),
            ));
        }
        lines
    }

    fn choice_line(&self, index: usize, first_visible: bool) -> Line<'static> {
        let choice = &self.choices[index];
        let active = self.cursor == index;
        let name_style = if active {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            dim()
        };
        let lead =
            if first_visible || index == 0 || self.choices[index - 1].heading != choice.heading {
                choice.heading.as_str()
            } else {
                ""
            };
        let marker = if active { "▸ " } else { "  " };
        Line::from(vec![
            Span::styled(format!("{INDENT}{marker}{}", LABEL.left(lead)), dim()),
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
    fn detail_lines(&self, width: usize) -> (Vec<Line<'static>>, usize) {
        match &self.mode {
            Mode::Choosing | Mode::Running(None | Some(LoginPhase::ExchangingCode)) => {
                (Vec::new(), 0)
            }
            Mode::Running(Some(LoginPhase::AwaitingBrowser { url })) => {
                (field("open", url, dim(), width), 1)
            }
            Mode::Running(Some(LoginPhase::AwaitingDevice {
                user_code,
                url,
                expires_in,
            })) => {
                // The transcribable value and its lifetime precede the optional
                // link/track, so even a short viewport keeps them in reach.
                let mut lines = field(
                    "code",
                    user_code,
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                    width,
                );
                lines.extend(field("expires", expires_in, dim(), width));
                let required = lines.len();
                lines.extend(field("open", url, dim(), width));
                (lines, required)
            }
            Mode::Failed(reason) => (field("error", reason, Style::default().fg(RED), width), 1),
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
    if width < head[0].width() + 8 {
        // Stack the label when its fixed column would leave no readable value.
        // Keep the same Unicode-aware fold, without clipping code characters.
        let mut rows = vec![Line::styled(label.to_string(), dim())];
        rows.extend(text::wrap_line(
            &Line::styled(text.to_string(), style),
            width.max(1),
        ));
        return rows;
    }
    text::hang(&head, vec![Span::styled(text.to_string(), style)], width)
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

pub(super) fn login(app: &mut App) {
    app.open(Overlay::Login(LoginOverlay::new(false)));
}

/// Cancelling closes immediately and trips the flow's flag. Sliced waits
/// observe it within a tick; in-flight HTTP retains its timeout. Cancelled
/// jobs are ignored.
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

/// Attach the committed owner; a delayed UI job cannot restore an old token.
fn apply_login(
    app: &mut App,
    ctx: &CommandCtx<'_>,
    login: &oauth::CredentialHandle,
    replaced: bool,
) {
    let admitted = ctx.bureau.admit(login);
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
    let text = format!("[{action} {label}{next}]");
    app.tabs
        .focused_recorder()
        .emit_or_end(crate::record::Forensic::SystemNote { text });
}

#[cfg(test)]
mod tests {
    use super::*;
    use oauth::Grant;
    use ratatui::{Terminal, backend::TestBackend};

    fn token(login: Login, issued: &str, handle: &str) -> OAuthToken {
        OAuthToken {
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            issued: issued.into(),
            email: Some(handle.into()),
            expires_at: u64::MAX,
            grant: match login {
                Login::Chatgpt => Grant::Chatgpt {
                    id_token: "synthetic-id".into(),
                    client_id: "synthetic-client".into(),
                },
                Login::Codex => Grant::Codex {
                    account_id: "backend-id".into(),
                    workspace: None,
                    plan: None,
                },
                Login::Grok => Grant::Grok,
            },
        }
    }

    fn roster(per_service: usize) -> Vec<OAuthToken> {
        Login::ALL
            .into_iter()
            .flat_map(|login| {
                (0..per_service)
                    .rev()
                    .map(move |i| token(login, &format!("id-{i}"), &format!("user-{i}")))
            })
            .collect()
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(overlay: &LoginOverlay, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| overlay.render(frame, frame.area()))
            .expect("render overlay");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn choices_group_services_new_methods_and_sorted_renewals() {
        let overlay = LoginOverlay::with_tokens(&roster(2), false);
        let choices = &overlay.choices;
        assert_eq!(choices.len(), 12);
        let mut index = 0;
        for login in Login::ALL {
            for &method in oauth::methods(login) {
                assert!(
                    matches!(choices[index].sign_in, SignIn::New(l, m) if l == login && m == method)
                );
                assert_eq!(choices[index].label, "new account");
                assert_eq!(choices[index].heading, login.service().name.to_string());
                index += 1;
            }
            for id in ["id-0", "id-1"] {
                for &method in oauth::methods(login) {
                    assert!(matches!(&choices[index].sign_in,
                        SignIn::Renew(token, m) if token.login() == login && token.issued == id && *m == method));
                    assert!(choices[index].blurb.starts_with("sign in again · "));
                    index += 1;
                }
            }
        }
    }

    #[test]
    fn same_handle_renewals_sort_by_full_id_and_offer_both_codex_methods() {
        let overlay = LoginOverlay::with_tokens(
            &[
                token(Login::Codex, "z", "shared"),
                token(Login::Codex, "a", "shared"),
            ],
            true,
        );
        let renewals: Vec<_> = overlay
            .choices
            .iter()
            .filter_map(|choice| match &choice.sign_in {
                SignIn::Renew(token, method) => Some((token.issued.as_str(), *method)),
                SignIn::New(..) => None,
            })
            .collect();
        assert_eq!(
            renewals,
            vec![
                ("a", Method::Browser),
                ("a", Method::Device),
                ("z", Method::Browser),
                ("z", Method::Device),
            ]
        );
        assert!(
            overlay
                .choices
                .iter()
                .filter(|choice| matches!(choice.sign_in, SignIn::Renew(..)))
                .all(|choice| choice.label.contains("openai-codex:"))
        );
    }

    #[test]
    fn bounded_viewport_follows_focus_and_wraps_with_repeated_headings() {
        let mut overlay = LoginOverlay::with_tokens(&roster(8), false);
        let count = overlay.choices.len();
        for _ in 0..count * 2 {
            let lines = overlay.body_lines(64, 5);
            assert_eq!(lines.len(), 5);
            let visible = text(&lines);
            assert_eq!(visible.matches("▸").count(), 1);
            assert!(visible.contains(&overlay.choices[overlay.cursor].label));
            let start = overlay.cursor.saturating_sub(3);
            assert!(text(&lines[..1]).contains(&overlay.choices[start].heading));
            assert!(visible.contains("above") && visible.contains("below"));
            let normal = render(&overlay, 80, 24);
            assert_eq!(normal.matches("▸").count(), 1);
            assert!(normal.contains(&overlay.choices[overlay.cursor].label));
            assert!(render(&overlay, 32, 10).contains("▸"));
            overlay.key(KeyCode::Down);
        }
        assert_eq!(overlay.cursor, 0);
        overlay.key(KeyCode::Up);
        assert_eq!(overlay.cursor, count - 1);
        assert!(text(&overlay.body_lines(64, 1)).contains("▸"));
        assert_eq!(overlay.body_lines(64, 0), Vec::<Line<'_>>::new());
        overlay.key(KeyCode::Tab);
        assert_eq!(overlay.cursor, 0);
    }

    #[test]
    fn actual_terminal_render_keeps_device_code_and_expiry_with_six_accounts() {
        let mut overlay = LoginOverlay::with_tokens(&roster(2), false);
        overlay.cursor = 6;
        assert!(matches!(
            overlay.key(KeyCode::Enter),
            LoginAction::Start(SignIn::Renew(_, Method::Device))
        ));
        overlay.set_phase(LoginPhase::AwaitingDevice {
            user_code: "ABCD-EFGH".into(),
            url: "https://example.test/device".into(),
            expires_in: "15 minutes".into(),
        });
        let normal = render(&overlay, 80, 24);
        assert!(normal.contains("ABCD-EFGH"));
        assert!(normal.contains("15 minutes"));
        assert!(normal.contains("code") && normal.contains("expires") && normal.contains("open"));
        assert_eq!(normal.matches("▸").count(), 1);
        assert!(!normal.contains("new account"));
        for (width, height) in [(48, 12), (22, 8), (8, 3), (1, 1)] {
            render(&overlay, width, height);
        }
        overlay.set_failed("A synthetic failure".into());
        let failed = render(&overlay, 80, 24);
        assert_eq!(failed.matches("▸").count(), 1);
        assert!(failed.contains("A synthetic failure"));
        assert!(!failed.contains("new account"));
        overlay.key(KeyCode::Enter);
        for _ in 0..overlay.choices.len() {
            assert!(render(&overlay, 80, 24).contains("▸"));
            render(&overlay, 32, 10);
            overlay.key(KeyCode::Down);
        }
    }

    #[test]
    fn clipboard_value_and_hint_follow_only_awaiting_phases() {
        let mut overlay = LoginOverlay::with_tokens(&[], false);
        overlay.set_phase(LoginPhase::AwaitingBrowser {
            url: "https://example.test/browser".into(),
        });
        assert!(
            matches!(overlay.key(KeyCode::Char('y')), LoginAction::Yank(value) if value == "https://example.test/browser")
        );
        assert!(overlay.hint().contains("y copy"));
        overlay.yanked = true;
        assert!(overlay.hint().contains("copied"));
        overlay.set_phase(LoginPhase::AwaitingDevice {
            user_code: "DEVICE-CODE".into(),
            url: "https://example.test/device".into(),
            expires_in: "5 minutes".into(),
        });
        assert!(!overlay.yanked);
        assert!(
            matches!(overlay.key(KeyCode::Char('y')), LoginAction::Yank(value) if value == "DEVICE-CODE")
        );
        assert!(overlay.hint().contains("y copy"));
        overlay.set_phase(LoginPhase::ExchangingCode);
        assert!(matches!(overlay.key(KeyCode::Char('y')), LoginAction::None));
        assert!(!overlay.hint().contains("copy"));
    }

    #[test]
    fn unicode_device_values_wrap_under_their_label() {
        let code = "界".repeat(40);
        let lines = field("code", &code, dim(), 40);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| line.width() <= 40));
        let wrapped = text(&lines);
        assert!(wrapped.contains("code"));
        assert_eq!(wrapped.matches('界').count(), 40);
        let narrow = field("code", &code, dim(), 12);
        assert!(narrow.iter().all(|line| line.width() <= 12));
        assert_eq!(text(&narrow).matches('界').count(), 40);
    }
}
