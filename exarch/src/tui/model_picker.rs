//! The model picker's orchestration, over a live session (`/model`) or alone
//! on the screen before one exists (a launch with no model to restore).
//!
//! [`Picker`] is display and input only; this module, its one caller, reaches
//! the [`Bureau`] holding the credentials, the model catalog, and the network
//! seam, and turns a [`Pick`] into a live provider swap plus a saved
//! [`state::State`]. [`super::login`] mirrors the split.

use crate::provider::identity::{self, Account};
use crate::provider::listing::{Fetches, Listing};
use crate::provider::models::{ModelSource, ProviderEndpoint};
use crate::provider::{Bureau, Tuning, pricing, state};

use super::app::Overlay;
use super::picker::{self, Pick, Picker};
use super::terminal::TerminalGuard;
use super::tui_loop::{CommandCtx, OverlayTick, Tui, overlay_key, overlay_tick};

/// Where a picker is drawn and its keys read: over a live session, or alone.
trait Stage {
    fn picker(&mut self) -> Option<&mut Picker>;
    fn tick(&mut self) -> OverlayTick;
}

impl Stage for Tui {
    fn picker(&mut self) -> Option<&mut Picker> {
        self.app.picker_mut()
    }

    fn tick(&mut self) -> OverlayTick {
        overlay_tick(self)
    }
}

/// The picker alone on the screen, before any session exists.
struct Alone<'a> {
    screen: &'a mut TerminalGuard,
    picker: Picker,
}

impl Stage for Alone<'_> {
    fn picker(&mut self) -> Option<&mut Picker> {
        Some(&mut self.picker)
    }

    fn tick(&mut self) -> OverlayTick {
        let picker = &self.picker;
        if self
            .screen
            .term()
            .draw(|f| picker.render(f, f.area()))
            .is_err()
        {
            return OverlayTick::TerminalLost;
        }
        overlay_key()
    }
}

/// A picker over `accounts` opened on `tuning`, seeded from whatever the
/// catalog already holds, and the listing that fetches the rest. `None` when
/// the bureau is scripted and has no catalog to list from.
fn open(bureau: &Bureau, accounts: Vec<Account>, tuning: &Tuning) -> Option<(Picker, Listing)> {
    let ids = accounts.iter().map(|account| account.id.clone()).collect();
    let listing = bureau.with_catalog(|catalog| Listing::open(ids, catalog))?;
    let mut picker = Picker::new(accounts, tuning, pricing::caps_or_default);
    for (id, state) in listing.states() {
        picker.set_models(id, state.clone());
    }
    Some((picker, listing))
}

pub(super) fn pick_model(tui: &mut Tui, ctx: &CommandCtx<'_>) {
    // Open on the focused agent's live tuning; a settled one falls back to the
    // defaults.
    let tuning = tui
        .app
        .tabs
        .focused_agent()
        .map(|agent| agent.current_provider().tuning().clone())
        .unwrap_or_default();
    let Some((picker, listing)) = open(ctx.bureau, ctx.bureau.available(), &tuning) else {
        return;
    };
    tui.app.overlay = Some(Overlay::Picker(picker));
    let pick = drive(tui, ctx.bureau, listing);
    tui.app.overlay = None;
    if let Some(pick) = pick {
        apply_model_switch(tui, ctx, &pick);
    }
}

/// Choose a model before any session exists: the picker alone on `screen`,
/// over `accounts`, opened on `tuning`, and saying `why` when that is news.
///
/// # Errors
/// If the bureau is scripted and so lists nothing, or if the user backs out.
pub fn choose(
    screen: &mut TerminalGuard,
    bureau: &Bureau,
    accounts: Vec<Account>,
    tuning: &Tuning,
    why: Option<String>,
) -> Result<Pick, String> {
    let (picker, listing) =
        open(bureau, accounts, tuning).ok_or("a scripted session has no models to choose from")?;
    let mut alone = Alone {
        screen,
        picker: picker.noting(why),
    };
    drive(&mut alone, bureau, listing).ok_or_else(|| "no model chosen".to_string())
}

/// Poll keys and landed fetches until the picker resolves; `None` on cancel.
///
/// The catalog is taken per fold rather than held across the loop: the
/// listing's own fetches run on their own threads, so no frame holds a bureau
/// lock while one of them is out.
fn drive(stage: &mut impl Stage, bureau: &Bureau, mut listing: Listing) -> Option<Pick> {
    // Spawned from inside the loop, unlike `listing`, whose fetches are all away
    // before it.
    let mut endpoints: Fetches<String, Vec<ProviderEndpoint>> = Fetches::new();
    loop {
        // The picker's copy is for render; `listing` stays authoritative.
        let woken = bureau
            .with_catalog(|catalog| listing.pump(catalog))
            .unwrap_or_default();
        for id in woken {
            if let (Some(state), Some(p)) = (listing.state(&id), stage.picker()) {
                p.set_models(&id, state.clone());
            }
        }
        for (model, result) in endpoints.landed() {
            let state = match result {
                Ok(list) => {
                    let _ = bureau
                        .with_catalog(|catalog| catalog.record_endpoints(&model, list.clone()));
                    picker::EndpointsState::Loaded(list)
                }
                Err(reason) => picker::EndpointsState::Failed(reason),
            };
            if let Some(p) = stage.picker() {
                p.set_endpoints(&model, state);
            }
        }
        // Seeding the state is also the dedup: the next poll no longer reports
        // this model as needing a fetch.
        let needed = stage
            .picker()
            .and_then(|p| p.focused_or_model_needing_endpoints());
        if let Some(model) = needed {
            let cached = bureau
                .with_catalog(|catalog| catalog.cached_endpoints(&model))
                .flatten();
            if let Some(list) = cached {
                if let Some(p) = stage.picker() {
                    p.set_endpoints(&model, picker::EndpointsState::Loaded(list));
                }
            } else {
                if let Some(p) = stage.picker() {
                    p.set_endpoints(&model, picker::EndpointsState::Loading);
                }
                // The seam is cloned out and the fetch runs on its own thread,
                // so the catalog is free again before the network is touched.
                if let Some(source) = bureau.with_catalog(|catalog| catalog.source().clone()) {
                    endpoints.spawn(model.clone(), move || source.endpoints(&model));
                }
            }
        }
        match stage.tick() {
            OverlayTick::TerminalLost | OverlayTick::Cancel => return None,
            OverlayTick::Idle => {}
            OverlayTick::Key(code) => {
                if let Some(pick) = stage.picker()?.key(code) {
                    return Some(pick);
                }
            }
        }
    }
}

/// Rebuild the provider for `pick` and swap it into the *focused* agent's
/// handle, which its next turn reads; a failed persist leaves that switch
/// standing. The switch reaches the trace as a [`Forensic::ModelChanged`] and
/// the screen as the status bar's live label — never as transcript chatter;
/// its own failures are view chrome.
///
/// [`Forensic::ModelChanged`]: crate::record::Forensic::ModelChanged
fn apply_model_switch(tui: &mut Tui, ctx: &CommandCtx<'_>, pick: &Pick) {
    // Every failure below answers one gesture on this tab, so it lands here —
    // the persist too, whose file is project-wide but whose message is not.
    let focused = tui.app.tabs.focused();
    let available = ctx.bureau.available();
    // A tab that settled while the picker was open has no handle to swap.
    let Some(agent) = tui.app.tabs.agent(focused) else {
        tui.app
            .push_error(focused, "the focused agent is no longer live");
        return;
    };
    let provider = agent.provider.clone();
    // The token override is no part of the selection, so it rides across by hand.
    let built = match ctx.bureau.build(
        &pick.account,
        pick.model.clone(),
        &pick.tuning,
        pick.route.clone(),
        provider.current().max_tokens_override(),
    ) {
        Ok(built) => built,
        Err(e) => {
            tui.app.push_error(focused, &e);
            return;
        }
    };
    let saved = state::State::of(&built, &available);
    let context_window = built.context_window();
    provider.swap(built);
    tui.app.update_live_model(&provider.current(), &available);
    let state_dir = crate::app::EXARCH.project_dir(ctx.info.cwd);
    if let Err(e) = state::save(&state_dir, &saved) {
        tui.app
            .push_error(focused, &format!("could not persist selection: {e}"));
    }
    if let Err(error) = ctx.recorder.emit(crate::record::Forensic::ModelChanged {
        model: pick.model.clone(),
        context_window,
        label: identity::label(&pick.account, &available),
        service: Some(pick.account.service.name.as_str().to_string()),
        account: Some(pick.account.id.as_str().to_string()),
    }) {
        ctx.recorder.report_fault(&error);
    }
}
