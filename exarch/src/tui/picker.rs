//! The `/model` tuning overlay — a floating, bezel-framed modal.
//!
//! The rest of the TUI is a flat stack of strips; this picker is the one
//! deliberate exception — a Norton-Commander-flavoured overlay that floats
//! above the (dimmed) session while open, drawn last over a [`Clear`]ed
//! centre of the frame. It is modal in *behaviour* (an early-return guard in
//! [`super::App::key`] and its own [`drive_picker`](super::drive_picker) loop)
//! and modal in *rendering*.
//!
//! It edits four things at once — the *selection* and *today's tuning*:
//!
//! * **Model** (nominal data) → **position**: a fuzzy-filtered vertical list
//!   of `provider / model` rows, ranked by `nucleo_matcher`, the selected row
//!   reversed.
//! * **Effort** (ordered data) → **value + size**: a rung ladder
//!   `auto · none · low · med · high · xhigh · max`, drawn as an ascending
//!   block ramp whose glyph height *and* brightness grow with the rung — the
//!   canonical Bertin encoding for ordered magnitude.
//! * **Temperature** (quantitative data) → **hue + size**: a track coloured by
//!   a cold-blue→warm-red gradient (literally apt) and filled to length, so
//!   both the position-of-hue and the fill encode the value.
//! * **Top-p** (quantitative data) → **size**: a single-hue track — top-p is
//!   nucleus *mass*, not a temperature, so it earns no gradient; the fill
//!   (length) alone encodes the value, the same chassis as the temperature
//!   track with the hue held constant.
//!
//! When the highlighted model is an `OpenRouter` `vendor/model`, a fifth control
//! appears — the **provider** row → **hue**: the upstream serving providers
//! `OpenRouter` lists for *that* model (`DeepInfra`, Novita, …), one hue-coded tag
//! each annotated with its context window and quantization, plus an `auto`
//! default. Choosing one pins the request's `provider.order` route; it is a
//! routing choice, not a filter, so it never moves the highlighted model. The
//! row is inert (and skipped by `Tab`) for any non-OpenRouter model — only
//! `OpenRouter` routes.
//!
//! Which field has the keyboard is itself shown with **value**: the active
//! field brightens, the others dim. `Tab`/`BackTab` cycle the field; typing
//! always routes to the search box; `Enter` applies the whole selection
//! (model + tuning + route) from any field; `Esc` dismisses.
//!
//! The tuning rows gate themselves on the *highlighted* model's catalog
//! `supported_parameters`: a model that doesn't admit `reasoning`,
//! `temperature`, or `top_p` grays that row out, ignores its arrows, and masks
//! the knob out of the emitted [`Tuning`] so it is never sent — but the picker
//! keeps the user's setting for the next model that does admit it. An unknown
//! model (a
//! catalog miss, or the manual row) reads as supported, so a row only grays
//! when the catalog *positively* reports the parameter absent.
//!
//! The picker is a pure display+input component: it holds the query, the
//! per-provider model lists and per-model serving-provider lists as they
//! arrive, the selection, the live tuning, and the chosen route. Fetching lives
//! in the REPL (which owns the credential-backed catalog and the network seam);
//! the REPL feeds results in via [`Picker::set_models`] and
//! [`Picker::set_endpoints`], so a list shows "loading…" until its background
//! fetch lands, and seeds the tuning from the focused provider's live values
//! via [`Picker::new`]. The serving-provider fetch is intent-driven: the REPL
//! reads [`Picker::focused_or_model_needing_endpoints`] and fetches a model's
//! providers only once the provider control is focused on it.

use super::line;
use super::palette::{BANNER_GOLD, CYAN, OVERLAY_BG, RED, SLATE};
use crate::models::ProviderEndpoint;
use crate::provider::Subscription;
use crate::provider::{ProviderId, ProviderKind, ReasoningEffort, Tuning};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use std::cmp::Reverse;
use std::collections::BTreeMap;

/// One provider's model-list fetch state.
pub enum ModelsState {
    /// The background fetch is in flight — the row reads "loading…".
    Loading,
    /// A usable list (from cache or a completed fetch).
    Loaded(Vec<String>),
    /// The fetch failed; the provider still accepts a manual model entry.
    Failed(String),
}

/// One model's serving-provider (`OpenRouter` `/endpoints`) fetch state. Keyed by
/// model id, fetched intent-driven when the provider control is focused on it.
pub enum EndpointsState {
    /// The background fetch is in flight — the row reads "loading…".
    Loading,
    /// The serving providers `OpenRouter` lists for this model.
    Loaded(Vec<ProviderEndpoint>),
    /// The fetch failed; the route stays `auto` (`OpenRouter` decides).
    Failed(String),
}

/// The chosen serving provider together with the model it was chosen for. The
/// control shows it (and [`Picker::apply`] emits it) only while that model is
/// the highlighted one — moving to another model leaves it inactive, so a route
/// never rides a model whose providers it was not chosen from.
struct Route {
    model: String,
    slug: String,
}

/// What a key press resolved to. The REPL acts on the non-`None` outcomes:
/// rebuilding the provider with the chosen model *and* tuning, persisting the
/// selection, and updating the status bar, or closing the picker.
pub enum PickAction {
    /// Keep the picker open; redraw.
    None,
    /// A listed `provider / model` row was chosen, with the live tuning and the
    /// chosen `OpenRouter` serving-provider slug (`None` for auto / non-OpenRouter).
    Selected(ProviderId, String, Tuning, Option<String>),
    /// Enter on the synthetic manual row: take the raw query as a model
    /// name and let the REPL resolve its provider (the listing-or-name
    /// fallback) — the escape hatch when a fetch failed or the wanted model
    /// is not listed. Carries the live tuning and route too (the route is
    /// `None` here — a manual model has no fetched serving-provider list).
    Manual(String, Tuning, Option<String>),
    /// Esc — dismiss without switching.
    Cancelled,
}

/// A row in the rendered list: either a listed model or the synthetic
/// manual-entry row.
enum Row {
    Model(ProviderId, String),
    Manual(String),
}

/// Which control holds the keyboard. The active field renders at full value;
/// the others dim. `Tab` cycles forward through this order, `BackTab` back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Search,
    Provider,
    Effort,
    Temperature,
    TopP,
}

/// One rung of the effort ladder: its short label, its ramp glyph (an
/// ascending block, so the ladder *grows* left-to-right), and the genai
/// [`ReasoningEffort`] it sends — `None` for "auto" (the option is not set and
/// the adapter's default stands). `auto` wears a dot rather than a block so it
/// reads as "no setting" rather than "the smallest setting".
struct Rung {
    label: &'static str,
    glyph: &'static str,
    effort: Option<ReasoningEffort>,
}

/// The effort ladder, ascending. `auto` first (the default, no option sent);
/// then genai's keyword rungs from `none` up to `max`. `XHigh`'s keyword is
/// `xhigh`; `Budget`/`Minimal` are intentionally omitted (the former carries a
/// raw token count with no place on an ordered ladder, the latter is a legacy
/// pre-gpt-5 alias for `low`).
const LADDER: &[Rung] = &[
    Rung {
        label: "auto",
        glyph: "·",
        effort: None,
    },
    Rung {
        label: "none",
        glyph: "▁",
        effort: Some(ReasoningEffort::None),
    },
    Rung {
        label: "low",
        glyph: "▂",
        effort: Some(ReasoningEffort::Low),
    },
    Rung {
        label: "med",
        glyph: "▄",
        effort: Some(ReasoningEffort::Medium),
    },
    Rung {
        label: "high",
        glyph: "▆",
        effort: Some(ReasoningEffort::High),
    },
    Rung {
        label: "xhigh",
        glyph: "▇",
        effort: Some(ReasoningEffort::XHigh),
    },
    Rung {
        label: "max",
        glyph: "█",
        effort: Some(ReasoningEffort::Max),
    },
];

/// Temperature bounds and step. genai accepts `0.0..=2.0`; the overlay steps
/// by tenths and treats "below zero" as a return to auto (unset).
const TEMP_MAX: f64 = 2.0;
const TEMP_STEP: f64 = 0.1;
/// Decimal places the temperature readout (and its snap) keep.
const TEMP_PLACES: usize = 1;
/// Top-p bounds and step. genai accepts `0.0..=1.0`; the overlay steps by
/// twentieths (so the common `0.9`/`0.95` land) and, like temperature, treats
/// "below zero" as a return to auto (unset).
const TOP_P_MAX: f64 = 1.0;
const TOP_P_STEP: f64 = 0.05;
/// Decimal places the top-p readout (and its snap) keep.
const TOP_P_PLACES: usize = 2;
/// Cells in a tuning track — the hue gradient and the fill both span this
/// width, shared by the temperature and top-p tracks.
const TRACK_W: usize = 24;

/// Rows of the model list visible at once.
const VISIBLE_ROWS: u16 = 8;
/// The overlay's fixed content width; clamped to the frame when narrower.
const OVERLAY_W: u16 = 74;

/// The overlay's interior margins — the Norton-Commander "airy" padding held
/// inside the bezel: [`PAD_X`] columns each side, [`PAD_Y`] rows top and
/// bottom, so the controls never crowd the double-line frame.
const PAD_X: u16 = 4;
const PAD_Y: u16 = 1;
/// The drop shadow's depth: the modal casts [`SHADOW_DEPTH`] columns of shade
/// to its right and one row below — the canonical Turbo-Vision/Norton-Commander
/// 2:1 lift (cells are ~2:1, so two columns read as square as one row) that
/// floats the overlay above the (dimmed) session.
const SHADOW_DEPTH: u16 = 2;
/// The shadow's foreground — cells under the shadow keep their glyph but are
/// repainted this near-black slate, so whatever lies beneath shows as a faint
/// silhouette rather than being blanked, the way Norton Commander inks its lift.
const SHADOW_FG: Color = Color::Rgb(14, 16, 22);

/// The temperature gradient endpoints (cold blue → warm red) and the effort
/// value-ramp endpoints (dim → bright cyan), interpolated by [`super::rail::mix`].
const COLD: Color = Color::Rgb(96, 160, 235);
const WARM: Color = Color::Rgb(228, 116, 96);
const EFFORT_DIM: Color = Color::Rgb(74, 86, 110);
const EFFORT_BRIGHT: Color = Color::Rgb(150, 205, 220);

/// The top-p track's single hue — top-p is nucleus *mass*, not a temperature,
/// so it carries no gradient; one constant colour across the track, the fill
/// (size) alone encoding the value. A muted green, kept distinct from the
/// effort ramp's cyan and the temperature track's blue→red.
const NUCLEUS: Color = Color::Rgb(140, 196, 150);

/// Snap `t` to `places` decimal places — keeps repeated `±step` additions from
/// drifting into float noise like `0.30000000000000004`.
fn snap(t: f64, places: usize) -> f64 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "places is a small decimal-place count"
    )]
    let scale = 10f64.powi(places as i32);
    (t * scale).round() / scale
}

/// Step a `0.0..=max` knob (temperature, top-p) by `step` in the `up`
/// direction, snapping to `places` decimals. `auto` (`None`) is the floor:
/// the first up-step lands on `0.0`, and stepping below zero returns to `auto`.
fn step_knob(value: Option<f64>, up: bool, step: f64, max: f64, places: usize) -> Option<f64> {
    if up {
        Some(snap(value.map_or(0.0, |t| (t + step).min(max)), places))
    } else {
        match value {
            None => None,
            Some(t) if t - step < -f64::EPSILON => None,
            Some(t) => Some(snap((t - step).max(0.0), places)),
        }
    }
}

/// A serving-provider tag's text: the provider name, then its context window
/// and quantization when reported — the magnitudes that tell apart providers of
/// the same model — wrapped in spaces for the reversed-block look the active tag
/// wears. Context length is humanised through the one shared token formatter so
/// `163840` reads `163.8k`, matching every other token readout.
fn provider_tag(endpoint: &ProviderEndpoint) -> String {
    let mut text = endpoint.provider_name.clone();
    if let Some(context_length) = endpoint.context_length {
        text.push(' ');
        text.push_str(&crate::provider::humanize_tokens(context_length));
    }
    if let Some(quantization) = &endpoint.quantization {
        text.push(' ');
        text.push_str(quantization);
    }
    format!(" {text} ")
}

/// Centre a `w × h` rect within `area`, clamped to fit.
fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Cast a Turbo-Vision drop shadow down-and-right of `area`: the cells one row
/// below and [`SHADOW_DEPTH`] columns to the right of the overlay are repainted
/// dark, keeping their glyph as a dim silhouette so the modal reads as lifted
/// off the session rather than punched into it. `cell_mut` bounds-checks, so
/// cells that fall off the frame are simply skipped.
fn render_shadow(f: &mut Frame, area: Rect) {
    let shadow = Style::default().fg(SHADOW_FG).bg(Color::Black);
    let buf = f.buffer_mut();
    let mut cast = |x: u16, y: u16| {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_style(shadow);
        }
    };
    // The bottom edge, shifted right by the depth so the corner squares off.
    for x in (area.x + SHADOW_DEPTH)..area.right().saturating_add(SHADOW_DEPTH) {
        cast(x, area.bottom());
    }
    // The right edge, SHADOW_DEPTH columns deep, starting one row down so the
    // top-right corner stays unshaded (the light falls from the upper left).
    for y in (area.y + 1)..=area.bottom() {
        for dx in 0..SHADOW_DEPTH {
            cast(area.right() + dx, y);
        }
    }
}

pub struct Picker {
    query: String,
    /// Available providers in declaration order; their lists fill in as
    /// fetches land.
    providers: Vec<ProviderId>,
    /// Each subscription-backed provider's plan flavour; their rows render
    /// the subscription-decorated label. A provider absent from the map is
    /// metered and renders its bare name.
    subscription: BTreeMap<ProviderId, Subscription>,
    models: BTreeMap<ProviderId, ModelsState>,
    /// Per-model serving-provider lists, keyed by `OpenRouter` model id, as they
    /// arrive from the REPL's intent-driven fetch. Absent until the provider
    /// control is first focused on that model.
    endpoints: BTreeMap<String, EndpointsState>,
    /// Index into the current filtered [`Self::rows`].
    selected: usize,
    /// Which control has the keyboard.
    focus: Focus,
    /// The chosen `OpenRouter` serving provider, paired with the model it was
    /// chosen for (see [`Route`]). Active — shown and emitted — only while that
    /// model is highlighted; `None` is "auto" (`OpenRouter` decides).
    route: Option<Route>,
    /// Index into [`LADDER`] — the chosen effort rung.
    effort_idx: usize,
    /// The chosen temperature, or `None` for auto (unset).
    temperature: Option<f64>,
    /// The chosen top-p, or `None` for auto (unset).
    top_p: Option<f64>,
    /// Capability lookup for the *highlighted* model: the tuning rows gate
    /// themselves on its `supported_parameters` so effort/temperature gray out
    /// (and stop being sent) on a model that doesn't admit them. Injected
    /// (production passes [`crate::pricing::caps_or_default`]) so the picker stays a
    /// pure component the tests can drive with a stub.
    caps: fn(&str) -> crate::pricing::ModelCaps,
}

impl Picker {
    /// Open over `providers`, all initially loading until the REPL feeds
    /// cached or fetched lists. `subscription` maps each plan-backed provider
    /// to its flavour, whose rows read as the subscription. `initial` seeds
    /// the effort/temperature controls from the focused provider's live
    /// tuning, so reopening shows the values currently in force.
    pub fn new(
        providers: Vec<ProviderId>,
        subscription: BTreeMap<ProviderId, Subscription>,
        initial: &Tuning,
        caps: fn(&str) -> crate::pricing::ModelCaps,
    ) -> Self {
        let models = providers
            .iter()
            .map(|id| (id.clone(), ModelsState::Loading))
            .collect();
        let effort_idx = LADDER
            .iter()
            .position(|r| match (&r.effort, &initial.effort) {
                (None, None) => true,
                (Some(a), Some(b)) => a.variant_name() == b.variant_name(),
                _ => false,
            })
            .unwrap_or(0);
        Self {
            query: String::new(),
            providers,
            subscription,
            models,
            endpoints: BTreeMap::new(),
            selected: 0,
            focus: Focus::Search,
            route: None,
            effort_idx,
            temperature: initial.temperature,
            top_p: initial.top_p,
            caps,
        }
    }

    /// The display label for `id`'s rows: the plain provider name, or
    /// the subscription-decorated form when it is on a plan.
    fn label(&self, id: &ProviderId) -> String {
        let subscription = self
            .subscription
            .get(id)
            .copied()
            .unwrap_or(Subscription::Metered);
        crate::provider::provider_label(subscription, id.label())
    }

    /// The providers whose lists are not yet known — the REPL spawns a
    /// background fetch for each on open.
    pub fn loading_providers(&self) -> Vec<ProviderId> {
        self.providers
            .iter()
            .filter(|id| matches!(self.models.get(id), Some(ModelsState::Loading)))
            .cloned()
            .collect()
    }

    /// Record a provider's resolved (or failed) list. Clamps the selection
    /// in case the visible list shrank.
    pub fn set_models(&mut self, id: &ProviderId, state: ModelsState) {
        self.models.insert(id.clone(), state);
        self.clamp_selection();
    }

    /// Record a model's serving-provider fetch result (or its in-flight /
    /// failed state), keyed by model id. Fed by the REPL's intent-driven fetch.
    pub fn set_endpoints(&mut self, model: &str, state: EndpointsState) {
        self.endpoints.insert(model.to_string(), state);
    }

    /// The `OpenRouter` model the provider control is focused on and whose serving
    /// providers have not yet been requested — the REPL's cue to spawn the
    /// fetch. `None` unless the provider control holds focus, an `OpenRouter`
    /// model is highlighted, and no [`EndpointsState`] is recorded for it yet
    /// (so seeding `Loading` before spawning naturally dedups the fetch).
    pub fn focused_or_model_needing_endpoints(&self) -> Option<String> {
        if self.focus != Focus::Provider {
            return None;
        }
        let model = self.highlighted_or_model(&self.rows())?;
        (!self.endpoints.contains_key(&model)).then_some(model)
    }

    /// Whether any provider's fetch is still in flight.
    pub fn is_loading(&self) -> bool {
        self.providers
            .iter()
            .any(|id| matches!(self.models.get(id), Some(ModelsState::Loading)))
    }

    /// The model on the highlighted row, if it is a listed model (the manual
    /// row and a still-empty list have no model to gate against). Reads the
    /// row slice the caller already computed for this frame or key.
    fn highlighted_model(&self, rows: &[Row]) -> Option<String> {
        match rows.get(self.selected) {
            Some(Row::Model(_, model)) => Some(model.clone()),
            _ => None,
        }
    }

    /// The highlighted model iff it is served through `OpenRouter` — the only
    /// provider that routes, so the only one the provider control applies to.
    fn highlighted_or_model(&self, rows: &[Row]) -> Option<String> {
        match rows.get(self.selected) {
            Some(Row::Model(id, model)) if id.famous() == Some(ProviderKind::Openrouter) => {
                Some(model.clone())
            }
            _ => None,
        }
    }

    /// The routing slugs the highlighted `OpenRouter` model's loaded endpoints
    /// offer, in listed order — the options the provider control cycles after
    /// `auto`. Empty while loading/failed/unfetched or for a non-OpenRouter row.
    fn endpoint_slugs(&self, rows: &[Row]) -> Vec<String> {
        let Some(model) = self.highlighted_or_model(rows) else {
            return Vec::new();
        };
        match self.endpoints.get(&model) {
            Some(EndpointsState::Loaded(endpoints)) => {
                endpoints.iter().map(|e| e.slug.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// The route slug in force *for the highlighted model* — `Some` only when a
    /// route was chosen and that same model is still highlighted, so the choice
    /// can never leak onto a different model. `None` is "auto".
    fn active_route(&self, rows: &[Row]) -> Option<&str> {
        let route = self.route.as_ref()?;
        (self.highlighted_or_model(rows).as_deref() == Some(route.model.as_str()))
            .then_some(route.slug.as_str())
    }

    /// Whether the highlighted model admits `param` (its catalog
    /// `supported_parameters`). Unknown models — a catalog miss, the manual
    /// row, or a still-loading list — read as supported, so the controls are
    /// only ever grayed when the catalog *positively* says the parameter is
    /// absent.
    fn supports(&self, rows: &[Row], param: &str) -> bool {
        match self.highlighted_model(rows) {
            Some(model) => (self.caps)(&model).supports(param),
            None => true,
        }
    }

    /// The live tuning the controls express *for the highlighted model* — a
    /// knob the model doesn't admit (`reasoning` / `temperature` absent from
    /// its catalog `supported_parameters`) is masked to `None` so it is neither
    /// sent nor persisted, while the picker keeps the user's setting for the
    /// next model that does admit it.
    fn tuning(&self, rows: &[Row]) -> Tuning {
        Tuning {
            effort: self
                .supports(rows, "reasoning")
                .then(|| LADDER[self.effort_idx].effort.clone())
                .flatten(),
            temperature: self
                .supports(rows, "temperature")
                .then_some(self.temperature)
                .flatten(),
            top_p: self.supports(rows, "top_p").then_some(self.top_p).flatten(),
        }
    }

    /// Handle one key press. Typing filters (and focuses the search box);
    /// `Tab`/`BackTab` cycle the field; the arrows move the focused control;
    /// `Enter` applies the model + tuning; `Esc` cancels.
    pub fn key(&mut self, code: ratatui::crossterm::event::KeyCode) -> PickAction {
        use ratatui::crossterm::event::KeyCode;
        match code {
            KeyCode::Esc => return PickAction::Cancelled,
            KeyCode::Enter => {
                let rows = self.rows();
                return self.apply(&rows);
            }
            KeyCode::Tab => {
                let rows = self.rows();
                self.focus = self.cycle(&rows, true);
            }
            KeyCode::BackTab => {
                let rows = self.rows();
                self.focus = self.cycle(&rows, false);
            }
            // Typing always means "filter models", whatever field had focus.
            // The chosen route self-gates on the highlighted model (see
            // [`Self::active_route`]), so a query that moves the highlight off
            // that model simply deactivates the route — no pruning needed.
            KeyCode::Char(c) => {
                self.focus = Focus::Search;
                self.query.push(c);
                self.selected = 0;
            }
            KeyCode::Backspace => {
                self.focus = Focus::Search;
                self.query.pop();
                self.selected = 0;
            }
            KeyCode::Up | KeyCode::Left => {
                let rows = self.rows();
                self.move_in_focus(&rows, false);
            }
            KeyCode::Down | KeyCode::Right => {
                let rows = self.rows();
                self.move_in_focus(&rows, true);
            }
            _ => {}
        }
        PickAction::None
    }

    /// Resolve the highlighted row into a selection carrying the live tuning and
    /// the route in force for that model.
    fn apply(&self, rows: &[Row]) -> PickAction {
        let route = self.active_route(rows).map(str::to_string);
        match rows.get(self.selected) {
            Some(Row::Model(id, model)) => {
                PickAction::Selected(id.clone(), model.clone(), self.tuning(rows), route)
            }
            Some(Row::Manual(query)) => {
                PickAction::Manual(query.clone(), self.tuning(rows), route)
            }
            None => PickAction::None,
        }
    }

    /// The next focus in cycle order
    /// (`Search → Provider → Effort → Temperature → TopP → …`). The provider
    /// control joins the cycle only when an `OpenRouter` model is highlighted —
    /// the only model the route applies to; for any other model it is skipped.
    fn cycle(&self, rows: &[Row], forward: bool) -> Focus {
        let has_provider = self.highlighted_or_model(rows).is_some();
        let order: &[Focus] = if has_provider {
            &[
                Focus::Search,
                Focus::Provider,
                Focus::Effort,
                Focus::Temperature,
                Focus::TopP,
            ]
        } else {
            &[
                Focus::Search,
                Focus::Effort,
                Focus::Temperature,
                Focus::TopP,
            ]
        };
        let pos = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (pos + 1) % order.len()
        } else {
            (pos + order.len() - 1) % order.len()
        };
        order[next]
    }

    /// Move the focused control: in Search the model selection, in Effort the
    /// rung, in Temperature the value. `up` is the increasing direction (down
    /// the list, up the ladder, warmer).
    fn move_in_focus(&mut self, rows: &[Row], up: bool) {
        match self.focus {
            Focus::Search => {
                if up {
                    let n = rows.len();
                    if n > 0 {
                        self.selected = (self.selected + 1).min(n - 1);
                    }
                } else {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            Focus::Provider => {
                // Cycle the highlighted OpenRouter model's serving providers
                // `auto → slug₀ → slug₁ → …`, clamping at the ends. This is a
                // routing choice for *that* model, so it deliberately leaves the
                // selection untouched — the highlighted model never moves.
                let Some(model) = self.highlighted_or_model(rows) else {
                    return;
                };
                let slugs = self.endpoint_slugs(rows);
                if slugs.is_empty() {
                    return; // loading / failed / none — nothing to cycle yet
                }
                // Index 0 is "auto"; index i+1 is slugs[i].
                let pos = self
                    .active_route(rows)
                    .and_then(|s| slugs.iter().position(|x| x == s))
                    .map_or(0, |i| i + 1);
                let next = if up {
                    (pos + 1).min(slugs.len())
                } else {
                    pos.saturating_sub(1)
                };
                self.route = (next > 0).then(|| Route {
                    model,
                    slug: slugs[next - 1].clone(),
                });
            }
            Focus::Effort => {
                if !self.supports(rows, "reasoning") {
                    return;
                }
                self.effort_idx = if up {
                    (self.effort_idx + 1).min(LADDER.len() - 1)
                } else {
                    self.effort_idx.saturating_sub(1)
                };
            }
            Focus::Temperature => {
                if !self.supports(rows, "temperature") {
                    return;
                }
                self.temperature =
                    step_knob(self.temperature, up, TEMP_STEP, TEMP_MAX, TEMP_PLACES);
            }
            Focus::TopP => {
                if !self.supports(rows, "top_p") {
                    return;
                }
                self.top_p = step_knob(self.top_p, up, TOP_P_STEP, TOP_P_MAX, TOP_P_PLACES);
            }
        }
    }

    /// The `(provider, model)` pairs matching the current fuzzy query, ranked
    /// by `nucleo_matcher` score (ties keeping listed order). An empty query
    /// yields every loaded model in listed order.
    fn query_matches(&self) -> Vec<(ProviderId, String)> {
        let q = self.query.trim();

        // (provider, model) pairs paired positionally with their haystacks.
        let mut candidates: Vec<(ProviderId, String)> = Vec::new();
        let mut haystacks: Vec<String> = Vec::new();
        for id in &self.providers {
            if let Some(ModelsState::Loaded(models)) = self.models.get(id) {
                let label = self.label(id);
                for model in models {
                    haystacks.push(format!("{label} / {model}"));
                    candidates.push((id.clone(), model.clone()));
                }
            }
        }

        if q.is_empty() {
            return candidates;
        }

        // Fuzzy-match each haystack, keeping its index so the row survives
        // even when two providers list the same model name.
        let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        // A fresh per-call matcher is cheap here (one keystroke produces one
        // pass over a small list); we cannot borrow a stored one mutably
        // through `&self`.
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut scored: Vec<(usize, u32)> = haystacks
            .iter()
            .enumerate()
            .filter_map(|(i, hay)| {
                pattern
                    .score(Utf32Str::new(hay, &mut buf), &mut matcher)
                    .map(|score| (i, score))
            })
            .collect();
        // Descending score; stable sort keeps listed order on ties.
        scored.sort_by_key(|(_, score)| Reverse(*score));
        scored
            .into_iter()
            .map(|(i, _)| candidates[i].clone())
            .collect()
    }

    /// The displayed rows: the query matches, plus a synthetic manual-entry row
    /// when the query is non-empty (so a model that's not listed, or a provider
    /// whose fetch failed, is still reachable). The provider control is a
    /// routing choice, not a filter, so it does not narrow this list.
    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .query_matches()
            .into_iter()
            .map(|(id, model)| Row::Model(id, model))
            .collect();
        let q = self.query.trim();
        if !q.is_empty() {
            rows.push(Row::Manual(q.to_string()));
        }
        rows
    }

    fn clamp_selection(&mut self) {
        let n = self.rows().len();
        self.selected = if n == 0 { 0 } else { self.selected.min(n - 1) };
    }

    /// Providers whose fetch failed, with their reasons — surfaced as dim
    /// notes so the absent models are explained and the manual-entry fallback
    /// is obvious.
    fn failures(&self) -> Vec<(&ProviderId, &str)> {
        self.providers
            .iter()
            .filter_map(|id| match self.models.get(id) {
                Some(ModelsState::Failed(reason)) => Some((id, reason.as_str())),
                _ => None,
            })
            .collect()
    }

    // --- rendering -----------------------------------------------------------

    /// The overlay's outer size: the fixed width (clamped to the frame) and a
    /// height that fits the search line, status line, bordered model list, the
    /// always-reserved serving-provider row, the three tuning rows, one note
    /// per failed provider, and the bezel.
    fn desired_size(&self, frame: Rect) -> (u16, u16) {
        let w = OVERLAY_W.min(frame.width);
        // Notes wrap to the inner text column: the overlay width less the two
        // bezel border cells and the horizontal padding on each side. Counting
        // the pre-wrapped lines reserves exactly the rows the render emits.
        let note_width = w.saturating_sub(2 + 2 * PAD_X);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "wrapped note-row count; bounded by the tiny catalog"
        )]
        let failed = self.failed_lines(note_width).len() as u16;
        // bezel(2) + airy pad(2·PAD_Y) + search(1) + status(1)
        //          + list(VISIBLE+border) + effort(1) + temp(1) + top-p(1)
        //          + failed notes (wrapped)
        // Always reserve the provider row so the overlay size is stable; the
        // row reads "OpenRouter routing only" for a non-OpenRouter model.
        let h = 2 + 2 * PAD_Y + 1 + 1 + (VISIBLE_ROWS + 2) + 1 + 1 + 1 + 1 + failed;
        (w, h.min(frame.height.max(3)))
    }

    /// Draw the floating overlay over the centre of `frame`: a double-line
    /// bezel (the "above the session" affordance) holding the search box, the
    /// fuzzy model list, the always-reserved serving-provider row, the effort
    /// ramp, the temperature and top-p tracks, and the function-key footer on
    /// the bottom border. The filtered row slice is computed once here and
    /// threaded through the row-reading helpers.
    pub fn render(&self, f: &mut Frame, frame: Rect) {
        let (w, h) = self.desired_size(frame);
        let area = centered(w, h, frame);
        let plane = Style::default().bg(OVERLAY_BG);
        let rows = self.rows();

        // Cast the drop shadow first (down-right of the box), then blank the
        // cells beneath the box itself and paint the bezel over them.
        render_shadow(f, area);
        f.render_widget(Clear, area);
        let bezel = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD))
            .style(plane)
            .title(
                Line::from(Span::styled(
                    " MODEL ",
                    plane.fg(BANNER_GOLD).add_modifier(Modifier::BOLD),
                ))
                .centered(),
            )
            .title_bottom(
                Line::from(Span::styled(
                    " ⇥ field · ↑↓ pick · ←→ adjust · ⏎ apply · esc cancel ",
                    plane.fg(SLATE).add_modifier(Modifier::DIM),
                ))
                .centered(),
            )
            .padding(Padding::new(PAD_X, PAD_X, PAD_Y, PAD_Y));
        let inner = bezel.inner(area);
        f.render_widget(bezel, area);

        let mut constraints = vec![
            Constraint::Length(1),                // search
            Constraint::Length(1),                // status
            Constraint::Length(VISIBLE_ROWS + 2), // bordered model list
        ];
        // Always reserve the provider row for layout stability.
        constraints.push(Constraint::Length(1)); // serving provider
        constraints.extend_from_slice(&[
            Constraint::Length(1), // effort
            Constraint::Length(1), // temperature
            Constraint::Length(1), // top-p
            Constraint::Min(0),    // failed-provider notes
        ]);
        let chunks = Layout::vertical(constraints).split(inner);
        let mut ci = 0; // chunk index
        f.render_widget(Paragraph::new(self.search_line()).style(plane), chunks[ci]);
        ci += 1;
        f.render_widget(Paragraph::new(self.status_line(&rows)).style(plane), chunks[ci]);
        ci += 1;
        self.render_list(f, &rows, chunks[ci], plane);
        ci += 1;
        // Always render the provider row (inert "OpenRouter routing only" for a
        // non-OpenRouter model), so the overlay's height stays stable.
        f.render_widget(
            Paragraph::new(self.provider_line(&rows, chunks[ci].width)).style(plane),
            chunks[ci],
        );
        ci += 1;
        f.render_widget(
            Paragraph::new(self.effort_line(self.supports(&rows, "reasoning"))).style(plane),
            chunks[ci],
        );
        ci += 1;
        f.render_widget(
            Paragraph::new(self.temp_line(self.supports(&rows, "temperature"))).style(plane),
            chunks[ci],
        );
        ci += 1;
        f.render_widget(
            Paragraph::new(self.top_p_line(self.supports(&rows, "top_p"))).style(plane),
            chunks[ci],
        );
        ci += 1;
        let notes = self.failed_lines(chunks[ci].width);
        if !notes.is_empty() {
            f.render_widget(Paragraph::new(notes).style(plane), chunks[ci]);
        }
    }

    /// A field label, bright when focused and dim otherwise — focus rendered
    /// as value, so the eye finds the live control by lightness.
    fn field_label(&self, text: &str, field: Focus) -> Span<'static> {
        let style = if self.focus == field {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE).add_modifier(Modifier::DIM)
        };
        Span::styled(format!("{text:<7}"), style)
    }

    fn search_line(&self) -> Line<'static> {
        let focused = self.focus == Focus::Search;
        let query_style = if focused {
            Style::default().fg(CYAN)
        } else {
            Style::default().fg(SLATE)
        };
        let caret = if focused { "▏" } else { " " };
        Line::from(vec![
            self.field_label("search", Focus::Search),
            Span::styled(self.query.clone(), query_style),
            Span::styled(caret, Style::default().fg(CYAN)),
        ])
    }

    fn status_line(&self, rows: &[Row]) -> Line<'static> {
        let n = rows.len();
        let text = if self.is_loading() {
            "loading…".to_string()
        } else {
            format!("{n} match{}", if n == 1 { "" } else { "es" })
        };
        Line::from(Span::styled(
            format!("{:<7}{text}", ""),
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        ))
    }

    /// The model list inside its own rounded panel (a panel-within-the-bezel,
    /// Norton-Commander style). The selected row is reversed; the panel border
    /// brightens when the search field has focus.
    fn render_list(&self, f: &mut Frame, rows: &[Row], area: Rect, plane: Style) {
        let focused = self.focus == Focus::Search;
        let border = if focused {
            Style::default().fg(CYAN)
        } else {
            Style::default().fg(SLATE).add_modifier(Modifier::DIM)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border)
            .style(plane);
        let list_area = block.inner(area);
        f.render_widget(block, area);

        let window = list_area.height as usize;
        // Scroll so the selected row stays visible.
        let start = self.selected.saturating_sub(window.saturating_sub(1));
        let lines: Vec<Line<'static>> = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(window)
            .map(|(i, row)| {
                let text = match row {
                    Row::Model(id, m) => format!("{} / {m}", self.label(id)),
                    Row::Manual(q) => format!("use “{q}” as a manual model"),
                };
                let mut style = match row {
                    Row::Manual(_) => Style::default().fg(SLATE).add_modifier(Modifier::ITALIC),
                    Row::Model(..) => Style::default().fg(CYAN),
                };
                if i == self.selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Line::from(Span::styled(text, style))
            })
            .collect();
        f.render_widget(Paragraph::new(lines).style(plane), list_area);
    }

    /// A grayed tuning row for a model that doesn't admit the parameter — the
    /// label plus a dim note, so the knob reads as deliberately disabled rather
    /// than missing.
    fn unsupported_row(label: &str) -> Line<'static> {
        let dim = Style::default().fg(SLATE).add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled(format!("{label:<7}"), dim),
            Span::styled(
                "— not supported by this model",
                dim.add_modifier(Modifier::ITALIC),
            ),
        ])
    }

    /// The serving-provider row, rendered for the highlighted model:
    /// * a non-OpenRouter model → a dim note (the row is inert; `Tab` skips it);
    /// * an `OpenRouter` model whose providers are still loading → "loading…";
    /// * a failed fetch → `auto`, with a dim note (the route stays auto);
    /// * loaded → an `auto` tag plus one hue-coded tag per serving provider,
    ///   each annotated with its context window and quantization (nominal data
    ///   earns hue, the magnitudes printed as values), the active one reversed.
    fn provider_line(&self, rows: &[Row], width: u16) -> Line<'static> {
        let focused = self.focus == Focus::Provider;
        let label = self.field_label("provider", Focus::Provider);
        let dim = Style::default().fg(SLATE).add_modifier(Modifier::DIM);
        // The `auto` tag's style: lit (cyan) when active and focused, a dim
        // reverse when active but unfocused, plain dim when not the choice.
        let auto_style = |active: bool| {
            if active && focused {
                Style::default()
                    .fg(CYAN)
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else if active {
                Style::default()
                    .fg(SLATE)
                    .add_modifier(Modifier::DIM | Modifier::REVERSED)
            } else {
                dim
            }
        };

        let Some(model) = self.highlighted_or_model(rows) else {
            return Line::from(vec![
                label,
                Span::styled(
                    "— OpenRouter routing only",
                    dim.add_modifier(Modifier::ITALIC),
                ),
            ]);
        };

        let endpoints: &[ProviderEndpoint] = match self.endpoints.get(&model) {
            Some(EndpointsState::Loaded(endpoints)) => endpoints,
            Some(EndpointsState::Loading) => {
                return Line::from(vec![label, Span::styled("loading…", dim)]);
            }
            Some(EndpointsState::Failed(reason)) => {
                return Line::from(vec![
                    label,
                    Span::styled(" auto ", auto_style(true)),
                    Span::styled(format!("  {reason}"), dim.add_modifier(Modifier::ITALIC)),
                ]);
            }
            // Not yet fetched (the control has not been focused on this model):
            // show `auto` alone — focusing the control triggers the fetch.
            None => &[],
        };

        // A palette of distinct hues for provider tags — nominal data earns hue,
        // the strongest Bertin variable for association.  Seven colours far
        // apart on the hue circle so neighbouring tags never blur.
        let colors: &[Color] = &[
            Color::Rgb(130, 190, 230), // blue
            Color::Rgb(230, 150, 120), // warm red
            Color::Rgb(130, 210, 150), // green
            Color::Rgb(210, 170, 110), // gold
            Color::Rgb(180, 140, 210), // violet
            Color::Rgb(110, 200, 200), // teal
            Color::Rgb(220, 140, 180), // rose
        ];
        // Index 0 is `auto`; index i+1 is `endpoints[i]`.
        let active = self
            .active_route(rows)
            .and_then(|slug| endpoints.iter().position(|e| e.slug == slug))
            .map_or(0, |i| i + 1);

        let mut spans = vec![label, Span::styled(" auto ", auto_style(active == 0))];
        for (i, endpoint) in endpoints.iter().enumerate() {
            let hue = colors[i % colors.len()];
            let style = if (i + 1) == active {
                Style::default()
                    .fg(hue)
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default().fg(hue).add_modifier(Modifier::DIM)
            };
            spans.push(Span::styled(provider_tag(endpoint), style));
        }
        let total: usize = spans.iter().map(Span::width).sum();
        if total <= width as usize {
            return Line::from(spans);
        }

        // The line overflows — keep the label and a window of tags around the
        // active one so the highlight never falls off the edge.
        let active_span = active + 1; // index in spans: 1 = auto, 2 = first tag, …
        let budget = width as usize - spans[0].width();

        let mut lo = active_span;
        let mut hi = active_span + 1; // exclusive
        let mut used = spans[active_span].width();

        while lo > 1 && used + spans[lo - 1].width() <= budget {
            lo -= 1;
            used += spans[lo].width();
        }
        while hi < spans.len() && used + spans[hi].width() <= budget {
            used += spans[hi].width();
            hi += 1;
        }

        let mut result = vec![spans[0].clone()];
        if lo > 1 {
            result.push(Span::styled(" … ", dim));
        }
        result.extend(spans[lo..hi].iter().cloned());
        if hi < spans.len() {
            result.push(Span::styled(" …", dim));
        }
        Line::from(result)
    }
    /// The effort ladder: an ascending block ramp, each rung brightening with
    /// its ordinal (value) as the glyph grows (size); the chosen rung is
    /// reversed and its label printed. Grayed when the highlighted model has
    /// no reasoning effort to set.
    fn effort_line(&self, supported: bool) -> Line<'static> {
        if !supported {
            return Self::unsupported_row("effort");
        }
        let focused = self.focus == Focus::Effort;
        #[allow(clippy::cast_precision_loss, reason = "const slice length")]
        let last = LADDER.len().saturating_sub(1).max(1) as f32;
        let mut spans = vec![self.field_label("effort", Focus::Effort)];
        for (i, rung) in LADDER.iter().enumerate() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "enumerate index over a small const-length slice"
            )]
            let value = super::rail::mix(EFFORT_DIM, EFFORT_BRIGHT, i as f32 / last);
            let mut style = Style::default().fg(value);
            if !focused {
                style = style.add_modifier(Modifier::DIM);
            }
            if i == self.effort_idx {
                style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
            }
            spans.push(Span::styled(rung.glyph, style));
        }
        let chosen = LADDER[self.effort_idx].label;
        let label_style = if focused {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE)
        };
        spans.push(Span::styled(format!("  {chosen}"), label_style));
        Line::from(spans)
    }

    /// A quantitative fill track for `field`: `TRACK_W` cells, each coloured by
    /// `hue(i)`, filled to `value`'s fraction of `max` (size), unset cells and
    /// an unfocused field dimmed; `auto` (value `None`) draws the whole track
    /// faint. The readout prints the value to `places` decimals, or `auto`.
    /// This is the chassis the temperature (cold→warm hue) and top-p
    /// (single-hue) tracks share, differing only in their per-cell hue.
    fn track_line(
        &self,
        label: &str,
        field: Focus,
        value: Option<f64>,
        max: f64,
        places: usize,
        hue: impl Fn(usize) -> Color,
    ) -> Line<'static> {
        let focused = self.focus == field;
        let mut spans = vec![self.field_label(label, field)];

        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "t/max in [0,1], TRACK_W is a small const"
        )]
        let filled = value.map_or(0, |t| ((t / max) * TRACK_W as f64).round() as usize);
        for i in 0..TRACK_W {
            let on = value.is_some() && i < filled;
            let glyph = if on { "█" } else { "░" };
            let mut style = Style::default().fg(hue(i));
            if !on || !focused {
                style = style.add_modifier(Modifier::DIM);
            }
            spans.push(Span::styled(glyph, style));
        }

        let readout = match value {
            Some(t) => format!("  {t:.places$}"),
            None => "  auto".to_string(),
        };
        let readout_style = if focused {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE)
        };
        spans.push(Span::styled(readout, readout_style));
        Line::from(spans)
    }

    /// The temperature track: cold→warm gradient (position-of-hue is literally
    /// apt) filled to length. Grayed when the highlighted model doesn't admit a
    /// temperature.
    fn temp_line(&self, supported: bool) -> Line<'static> {
        if !supported {
            return Self::unsupported_row("temp");
        }
        #[allow(clippy::cast_precision_loss, reason = "const track width")]
        let last = (TRACK_W - 1).max(1) as f32;
        #[allow(
            clippy::cast_precision_loss,
            reason = "enumerate index over a small const-length range"
        )]
        let hue = |i: usize| super::rail::mix(COLD, WARM, i as f32 / last);
        self.track_line(
            "temp",
            Focus::Temperature,
            self.temperature,
            TEMP_MAX,
            TEMP_PLACES,
            hue,
        )
    }

    /// The top-p track: a single [`NUCLEUS`] hue, the fill (size) alone
    /// encoding the value — top-p is nucleus mass, not a temperature, so it
    /// earns no gradient. Grayed when the highlighted model doesn't admit a
    /// top-p.
    fn top_p_line(&self, supported: bool) -> Line<'static> {
        if !supported {
            return Self::unsupported_row("top-p");
        }
        self.track_line(
            "top-p",
            Focus::TopP,
            self.top_p,
            TOP_P_MAX,
            TOP_P_PLACES,
            |_| NUCLEUS,
        )
    }

    /// Each failed-provider note word-wrapped to `width` columns, as a reddish
    /// warning block. The error `reason` is included in full and wrapped (not
    /// truncated) — a fetch failure's reason is the whole point of the note, so
    /// it must stay readable. A `⚠ ` marker opens the first row and
    /// continuations hang-indent under it, so a long reason reads as one block.
    /// Reuses [`line::push_wrapped`], the shared wrap-and-emit primitive, so the
    /// pre-wrapped lines double as the exact height [`Self::desired_size`]
    /// reserves.
    fn failed_lines(&self, width: u16) -> Vec<Line<'static>> {
        const MARKER: &str = "⚠ ";
        const HANG: &str = "  ";
        let style = Style::default().fg(RED).add_modifier(Modifier::BOLD);
        // Wrap to the column left of the marker/hang gutter so the marker and
        // its indent never push a row past `width`.
        let body_w = (width as usize).saturating_sub(MARKER.chars().count());
        let mut out = Vec::new();
        for (id, reason) in self.failures() {
            let text = format!(
                "{} — fetch failed: {reason} (type a model to enter manually)",
                id.label()
            );
            line::push_wrapped(&mut out, &text, body_w, |chunk, first| {
                let lead = if first { MARKER } else { HANG };
                Line::from(Span::styled(format!("{lead}{chunk}"), style))
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;
    use ratatui::crossterm::event::KeyCode;

    /// A famous provider's id — the common case in these tests.
    fn fam(kind: ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

    /// A custom provider's id with `label`.
    fn custom(label: &str) -> ProviderId {
        ProviderId::Custom(std::sync::Arc::new(crate::provider::CustomProvider {
            label: label.into(),
            key_env: format!("{}_KEY", label.to_uppercase()),
            endpoint: format!("https://{label}.example/v1/"),
            adapter: genai::adapter::AdapterKind::OpenAI,
        }))
    }

    /// A caps stub that knows nothing — an empty `supported_parameters` reads
    /// as "supports everything", so the tuning rows stay live (the common case
    /// these tests assume).
    fn caps_unknown(_: &str) -> crate::pricing::ModelCaps {
        crate::pricing::ModelCaps::default()
    }

    fn loaded_picker() -> Picker {
        let mut p = Picker::new(
            vec![fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)],
            BTreeMap::new(),
            &Tuning::default(),
            caps_unknown,
        );
        p.set_models(
            &fam(ProviderKind::Anthropic),
            ModelsState::Loaded(vec!["claude-opus-4".into(), "claude-haiku-4".into()]),
        );
        p.set_models(
            &fam(ProviderKind::Deepseek),
            ModelsState::Loaded(vec!["deepseek-chat".into()]),
        );
        p
    }

    /// A serving-provider endpoint fixture (context window and quantization
    /// elided — the cycling/route tests only read the slug).
    fn endpoint(name: &str, slug: &str) -> ProviderEndpoint {
        ProviderEndpoint {
            provider_name: name.into(),
            slug: slug.into(),
            context_length: None,
            quantization: None,
        }
    }

    /// An `OpenRouter` provider whose models carry `vendor/model` ids — the case
    /// the serving-provider control exists for.
    fn openrouter_picker() -> Picker {
        let mut p = Picker::new(
            vec![fam(ProviderKind::Openrouter)],
            BTreeMap::new(),
            &Tuning::default(),
            caps_unknown,
        );
        p.set_models(
            &fam(ProviderKind::Openrouter),
            ModelsState::Loaded(vec![
                "anthropic/claude-3".into(),
                "deepseek/deepseek-chat".into(),
                "deepseek/deepseek-r1".into(),
                "openai/gpt-5".into(),
            ]),
        );
        p
    }

    /// With no query every loaded model across providers is shown.
    #[test]
    fn empty_query_shows_all_loaded_models() {
        let p = loaded_picker();
        let labels: Vec<String> = p
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Model(id, m) => Some(format!("{} / {m}", id.label())),
                Row::Manual(_) => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "anthropic / claude-opus-4",
                "anthropic / claude-haiku-4",
                "deepseek / deepseek-chat",
            ]
        );
    }

    /// A subscription-backed provider's rows render the decorated label
    /// (`openai (ChatGPT subscription) / model`) while still matching a
    /// plain `openai` search, so the picker reads as the subscription and
    /// the search haystack keeps the bare provider name.
    #[test]
    fn subscription_provider_rows_carry_decorated_label() {
        let mut p = Picker::new(
            vec![fam(ProviderKind::Openai)],
            BTreeMap::from([(fam(ProviderKind::Openai), Subscription::ChatGpt)]),
            &Tuning::default(),
            caps_unknown,
        );
        p.set_models(
            &fam(ProviderKind::Openai),
            ModelsState::Loaded(vec!["gpt-5.5".into()]),
        );
        let labels: Vec<String> = p
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Model(id, m) => Some(format!("{} / {m}", p.label(&id))),
                Row::Manual(_) => None,
            })
            .collect();
        assert_eq!(labels, vec!["openai (ChatGPT subscription) / gpt-5.5"]);
        // The bare provider name still matches search.
        for c in "openai".chars() {
            p.key(KeyCode::Char(c));
        }
        let model_rows = p
            .rows()
            .into_iter()
            .filter(|r| matches!(r, Row::Model(..)))
            .count();
        assert_eq!(model_rows, 1);
    }

    /// A flat-rate provider (opencode Go) renders the generic
    /// `(subscription)` suffix — distinct from the `ChatGPT` plan's decoration
    /// — so the picker reads its plan correctly without claiming it is a
    /// `ChatGPT` login.
    #[test]
    fn flat_rate_provider_rows_carry_generic_subscription_label() {
        let mut p = Picker::new(
            vec![fam(ProviderKind::OpencodeGo)],
            BTreeMap::from([(fam(ProviderKind::OpencodeGo), Subscription::FlatRate)]),
            &Tuning::default(),
            caps_unknown,
        );
        p.set_models(
            &fam(ProviderKind::OpencodeGo),
            ModelsState::Loaded(vec!["glm-5.2".into()]),
        );
        let labels: Vec<String> = p
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Model(id, m) => Some(format!("{} / {m}", p.label(&id))),
                Row::Manual(_) => None,
            })
            .collect();
        assert_eq!(labels, vec!["opencode-go (subscription) / glm-5.2"]);
    }

    /// Typing filters by substring over the `provider / model` label, and a
    /// manual-entry row is appended once the query is non-empty.
    #[test]
    fn query_filters_substring_and_appends_manual_row() {
        let mut p = loaded_picker();
        for c in "haiku".chars() {
            p.key(KeyCode::Char(c));
        }
        let rows = p.rows();
        // One model match (haiku) plus the synthetic manual row.
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], Row::Model(_, m) if m == "claude-haiku-4"));
        assert!(matches!(&rows[1], Row::Manual(q) if q == "haiku"));
    }

    /// A provider filter narrows to that provider's models.
    #[test]
    fn provider_substring_narrows() {
        let mut p = loaded_picker();
        for c in "deepseek".chars() {
            p.key(KeyCode::Char(c));
        }
        let model_rows: Vec<_> = p
            .rows()
            .into_iter()
            .filter(|r| matches!(r, Row::Model(..)))
            .collect();
        assert_eq!(model_rows.len(), 1);
        assert!(matches!(
            &model_rows[0],
            Row::Model(id, _) if id.famous() == Some(ProviderKind::Deepseek)
        ));
    }

    /// Enter on a listed row yields `Selected(provider, model, tuning)`.
    #[test]
    fn enter_selects_highlighted_model() {
        let mut p = loaded_picker();
        // Move to the second row (anthropic / claude-haiku-4).
        p.key(KeyCode::Down);
        match p.key(KeyCode::Enter) {
            PickAction::Selected(id, m, _, _) if id.famous() == Some(ProviderKind::Anthropic) => {
                assert_eq!(m, "claude-haiku-4");
            }
            _ => panic!("expected Selected(anthropic, claude-haiku-4)"),
        }
    }

    /// A custom provider's models list and select through the picker exactly
    /// like a famous one: its declared label decorates the row and Enter
    /// yields the custom `ProviderId`.
    #[test]
    fn custom_provider_lists_and_selects() {
        let llama = custom("local-llama");
        let mut p = Picker::new(
            vec![llama.clone()],
            BTreeMap::new(),
            &Tuning::default(),
            caps_unknown,
        );
        p.set_models(&llama, ModelsState::Loaded(vec!["llama-3".into()]));
        let rows = p.rows();
        assert!(matches!(&rows[0], Row::Model(id, m) if id == &llama && m == "llama-3"));
        match p.key(KeyCode::Enter) {
            PickAction::Selected(id, m, _, _) => {
                assert_eq!(id, llama);
                assert_eq!(m, "llama-3");
            }
            _ => panic!("expected Selected(local-llama, llama-3)"),
        }
    }

    /// Enter on the manual row (a query matching nothing) yields the raw
    /// query for the REPL to resolve.
    #[test]
    fn enter_on_manual_row_yields_query() {
        let mut p = loaded_picker();
        for c in "claude-future-99".chars() {
            p.key(KeyCode::Char(c));
        }
        // Only the manual row matches; it is selected at index 0.
        match p.key(KeyCode::Enter) {
            PickAction::Manual(q, _, _) => assert_eq!(q, "claude-future-99"),
            _ => panic!("expected Manual(claude-future-99)"),
        }
    }

    /// Esc cancels.
    #[test]
    fn esc_cancels() {
        let mut p = loaded_picker();
        assert!(matches!(p.key(KeyCode::Esc), PickAction::Cancelled));
    }

    /// `Tab` cycles the focus Search → Effort → Temperature → `TopP` → Search,
    /// and the arrows then drive the focused control: in Effort they climb the
    /// ladder, in Temperature they warm the value, in `TopP` they fill the track.
    #[test]
    fn tab_cycles_focus_and_arrows_drive_the_focused_control() {
        let mut p = loaded_picker();
        assert_eq!(p.focus, Focus::Search);
        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Effort);
        // Up the ladder twice: auto → none → low.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(LADDER[p.effort_idx].label, "low");

        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Temperature);
        // From auto, one step right reaches 0.0, another 0.1.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(p.temperature, Some(0.1));

        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::TopP);
        // From auto, one step right reaches 0.0, another 0.05.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(p.top_p, Some(0.05));

        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Search);
    }

    /// Typing routes to the search box even when a tuning field had focus, so
    /// the model filter is always reachable.
    #[test]
    fn typing_refocuses_search() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Char('o'));
        assert_eq!(p.focus, Focus::Search);
        assert_eq!(p.query, "o");
    }

    /// Temperature steps in tenths, clamps at the maximum, and stepping below
    /// zero returns to auto (unset).
    #[test]
    fn temperature_steps_clamps_and_floors_to_auto() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Tab); // Temperature
        assert_eq!(p.temperature, None);
        // Three steps up: auto → 0.0 → 0.1 → 0.2.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(p.temperature, Some(0.2));
        // Down past zero returns to auto.
        p.key(KeyCode::Left);
        p.key(KeyCode::Left);
        assert_eq!(p.temperature, Some(0.0));
        p.key(KeyCode::Left);
        assert_eq!(p.temperature, None);
    }

    /// Top-p steps in twentieths, clamps at 1.0, and stepping below zero
    /// returns to auto (unset).
    #[test]
    fn top_p_steps_clamps_and_floors_to_auto() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Tab); // Temperature
        p.key(KeyCode::Tab); // TopP
        assert_eq!(p.top_p, None);
        // Three steps up: auto → 0.0 → 0.05 → 0.1.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(p.top_p, Some(0.1));
        // Down past zero returns to auto.
        p.key(KeyCode::Left);
        p.key(KeyCode::Left);
        assert_eq!(p.top_p, Some(0.0));
        p.key(KeyCode::Left);
        assert_eq!(p.top_p, None);
    }

    /// The chosen effort + temperature + top-p ride along with the selection.
    #[test]
    fn selection_carries_the_live_tuning() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Right); // auto → none
        p.key(KeyCode::Right); // none → low
        p.key(KeyCode::Right); // low → med
        p.key(KeyCode::Right); // med → high
        p.key(KeyCode::Tab); // Temperature
        p.key(KeyCode::Right); // auto → 0.0
        p.key(KeyCode::Right); // 0.0 → 0.1
        p.key(KeyCode::Tab); // TopP
        p.key(KeyCode::Right); // auto → 0.0
        p.key(KeyCode::Right); // 0.0 → 0.05
        match p.key(KeyCode::Enter) {
            PickAction::Selected(_, _, tuning, _) => {
                assert_eq!(
                    tuning.effort.as_ref().map(ReasoningEffort::variant_name),
                    Some("high")
                );
                assert_eq!(tuning.temperature, Some(0.1));
                assert_eq!(tuning.top_p, Some(0.05));
            }
            _ => panic!("expected Selected with tuning"),
        }
    }

    /// The controls open seeded from the focused provider's live tuning.
    #[test]
    fn opens_seeded_from_initial_tuning() {
        let p = Picker::new(
            vec![fam(ProviderKind::Anthropic)],
            BTreeMap::new(),
            &Tuning {
                effort: Some(ReasoningEffort::Medium),
                temperature: Some(0.5),
                top_p: Some(0.9),
            },
            caps_unknown,
        );
        assert_eq!(LADDER[p.effort_idx].label, "med");
        assert_eq!(p.temperature, Some(0.5));
        assert_eq!(p.top_p, Some(0.9));
    }

    /// A model whose catalog lists `temperature` but not `reasoning`: effort
    /// is masked out of the tuning and its arrows do nothing, while
    /// temperature stays live — and the masked effort is restored when a
    /// reasoning-capable model is highlighted again.
    fn caps_split(model: &str) -> crate::pricing::ModelCaps {
        let supported_parameters = if model == "chat-only" {
            vec!["temperature".to_string()]
        } else {
            vec!["reasoning".to_string(), "temperature".to_string()]
        };
        crate::pricing::ModelCaps {
            supported_parameters,
            ..Default::default()
        }
    }

    #[test]
    fn unsupported_effort_is_masked_and_remembered() {
        let mut p = Picker::new(
            vec![fam(ProviderKind::Anthropic)],
            BTreeMap::new(),
            &Tuning::default(),
            caps_split,
        );
        p.set_models(
            &fam(ProviderKind::Anthropic),
            ModelsState::Loaded(vec!["reasoner".into(), "chat-only".into()]),
        );

        // On the reasoning-capable row, set effort=high and temp=0.1.
        p.key(KeyCode::Tab); // Effort
        for _ in 0..4 {
            p.key(KeyCode::Right); // auto → none → low → med → high
        }
        p.key(KeyCode::Tab); // Temperature
        p.key(KeyCode::Right); // auto → 0.0
        p.key(KeyCode::Right); // 0.0 → 0.1
        let live = p.tuning(&p.rows());
        assert_eq!(live.effort.as_ref().map(ReasoningEffort::variant_name), Some("high"));
        assert_eq!(live.temperature, Some(0.1));

        // Highlight the chat-only model: effort masked out, temperature kept.
        p.key(KeyCode::Tab); // TopP
        p.key(KeyCode::Tab); // Search
        p.key(KeyCode::Down); // → chat-only
        let masked = p.tuning(&p.rows());
        assert!(masked.effort.is_none(), "reasoning masked for chat-only");
        assert_eq!(masked.temperature, Some(0.1));

        // Its effort arrows are inert.
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Left);
        assert_eq!(LADDER[p.effort_idx].label, "high", "rung unchanged");

        // Back on the reasoning model the setting returns.
        p.key(KeyCode::Tab); // Temperature
        p.key(KeyCode::Tab); // TopP
        p.key(KeyCode::Tab); // Search
        p.key(KeyCode::Up); // → reasoner
        assert_eq!(
            p.tuning(&p.rows()).effort.as_ref().map(ReasoningEffort::variant_name),
            Some("high")
        );
    }

    /// For a highlighted `OpenRouter` model with its serving providers loaded, Tab
    /// reaches the provider control and Right cycles `auto → deepinfra → novita`,
    /// clamping at the end; Left walks back to auto. Crucially, cycling the
    /// provider never moves the highlighted model, and the chosen slug rides Enter.
    #[test]
    fn provider_cycles_serving_endpoints_without_moving_the_model() {
        let mut p = openrouter_picker();
        p.key(KeyCode::Down); // highlight deepseek/deepseek-chat
        let model = "deepseek/deepseek-chat";
        assert_eq!(p.highlighted_model(&p.rows()).as_deref(), Some(model));
        p.set_endpoints(
            model,
            EndpointsState::Loaded(vec![
                endpoint("DeepInfra", "deepinfra"),
                endpoint("Novita", "novita"),
            ]),
        );

        p.key(KeyCode::Tab); // Search → Provider
        assert_eq!(p.focus, Focus::Provider);

        p.key(KeyCode::Right);
        assert_eq!(p.active_route(&p.rows()), Some("deepinfra"));
        assert_eq!(
            p.highlighted_model(&p.rows()).as_deref(),
            Some(model),
            "the highlighted model never moves when picking a provider"
        );
        p.key(KeyCode::Right);
        assert_eq!(p.active_route(&p.rows()), Some("novita"));
        // Right at the end clamps — it does not wrap back to auto.
        p.key(KeyCode::Right);
        assert_eq!(p.active_route(&p.rows()), Some("novita"));
        p.key(KeyCode::Left);
        assert_eq!(p.active_route(&p.rows()), Some("deepinfra"));
        p.key(KeyCode::Left);
        assert_eq!(p.active_route(&p.rows()), None);

        // The chosen route rides Enter alongside the highlighted model.
        p.key(KeyCode::Right); // auto → deepinfra
        match p.key(KeyCode::Enter) {
            PickAction::Selected(_, m, _, route) => {
                assert_eq!(m, model);
                assert_eq!(route.as_deref(), Some("deepinfra"));
            }
            _ => panic!("expected the highlighted model carrying its route"),
        }
    }

    /// The route is tied to the model it was chosen for: moving the highlight to
    /// another model deactivates it (so it can never ride a model whose
    /// providers it was not chosen from), and returning restores it.
    #[test]
    fn route_is_inactive_off_its_model_and_returns_on_it() {
        let mut p = openrouter_picker();
        p.key(KeyCode::Down); // deepseek/deepseek-chat
        let model = "deepseek/deepseek-chat";
        p.set_endpoints(
            model,
            EndpointsState::Loaded(vec![endpoint("DeepInfra", "deepinfra")]),
        );
        p.key(KeyCode::Tab); // Provider
        p.key(KeyCode::Right); // choose deepinfra
        assert_eq!(p.active_route(&p.rows()), Some("deepinfra"));

        // Return to the search field and move the highlight to another model.
        for _ in 0..4 {
            p.key(KeyCode::Tab); // Provider → Effort → Temperature → TopP → Search
        }
        assert_eq!(p.focus, Focus::Search);
        p.key(KeyCode::Down); // deepseek/deepseek-r1
        assert_ne!(p.highlighted_model(&p.rows()).as_deref(), Some(model));
        assert_eq!(
            p.active_route(&p.rows()),
            None,
            "the route does not ride another model"
        );

        // Back on its own model, the choice is active again.
        p.key(KeyCode::Up);
        assert_eq!(p.highlighted_model(&p.rows()).as_deref(), Some(model));
        assert_eq!(p.active_route(&p.rows()), Some("deepinfra"));
    }

    /// A non-OpenRouter model has no routing, so `Tab` skips the provider
    /// control (Search → Effort) and nothing requests endpoints for it.
    #[test]
    fn provider_control_skipped_for_non_openrouter_model() {
        let mut p = loaded_picker();
        assert!(p.highlighted_model(&p.rows()).is_some());
        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Effort);
        assert!(p.focused_or_model_needing_endpoints().is_none());
    }

    /// Focusing the provider control on an `OpenRouter` model is the cue to fetch
    /// its serving providers — and once the REPL seeds the in-flight state, the
    /// fetch is not requested again.
    #[test]
    fn focusing_provider_requests_endpoints_once() {
        let mut p = openrouter_picker(); // first row: anthropic/claude-3
        assert!(
            p.focused_or_model_needing_endpoints().is_none(),
            "nothing requested before the control is focused"
        );
        p.key(KeyCode::Tab); // Search → Provider
        assert_eq!(p.focus, Focus::Provider);
        assert_eq!(
            p.focused_or_model_needing_endpoints().as_deref(),
            Some("anthropic/claude-3")
        );
        p.set_endpoints("anthropic/claude-3", EndpointsState::Loading);
        assert!(
            p.focused_or_model_needing_endpoints().is_none(),
            "seeding Loading dedups the fetch"
        );
    }
}
