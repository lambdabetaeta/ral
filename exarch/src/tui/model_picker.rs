//! The `/model` overlay's orchestration.
//!
//! [`Picker`] is display and input only; this module, its one caller, owns the
//! credential store, the [`ModelCatalog`], and the network seam, and turns a
//! resolved [`picker::PickAction`] into a live [`Provider`] swap plus a saved
//! [`state::State`]. [`super::login`] mirrors the split.

use std::fmt::Write;
use std::sync::Arc;

use crate::provider::credential::CredentialStore;
use crate::provider::listing::{Fetches, Listing};
use crate::provider::models::{LiveSource, ModelCatalog, ModelSource, ProviderEndpoint};
use crate::provider::state;
use crate::provider::{self, Provider};

use super::app::Overlay;
use super::picker::{self, Picker};
use super::tui_loop::{CommandCtx, OverlayTick, Tui, overlay_tick};

pub(super) fn pick_model(tui: &mut Tui, ctx: &mut CommandCtx<'_>) {
    // A shared reborrow — only `/login`'s `add_oauth` needs the store mutably —
    // so `ctx` stays whole for `apply_model_switch`.
    let store = &*ctx.store;
    let available = store.available();
    // The picker's plan flavours; a provider absent from the map reads as metered.
    let subscription = available
        .iter()
        .filter_map(|id| {
            let kind = if store.is_subscription(id) {
                crate::provider::Subscription::ChatGpt
            } else if id.flat_rate() {
                crate::provider::Subscription::FlatRate
            } else {
                return None;
            };
            Some((id.clone(), kind))
        })
        .collect();
    // Open on the focused agent's live tuning; a settled one falls back to the
    // defaults.
    let initial_tuning = ctx
        .agents
        .provider(tui.app.tabs.focused())
        .map(|p| p.current().tuning().clone())
        .unwrap_or_default();
    let mut picker = Picker::new(
        available.clone(),
        subscription,
        &initial_tuning,
        crate::provider::pricing::caps_or_default,
    );
    // `Picker::new` seeded every row `Loading`, so only the rows `Listing::open`
    // settled from cache need forwarding; the misses land as `drive_picker` pumps.
    let listing = Listing::open(available, ctx.catalog);
    for (id, state) in listing.states() {
        match state {
            picker::ModelsState::Loaded(models) => {
                picker.set_models(id, picker::ModelsState::Loaded(models.clone()));
            }
            picker::ModelsState::Failed(reason) => {
                picker.set_models(id, picker::ModelsState::Failed(reason.clone()));
            }
            picker::ModelsState::Loading => {}
        }
    }
    tui.app.overlay = Some(Overlay::Picker(picker));
    let outcome = drive_picker(tui, store, ctx.catalog, listing);
    tui.app.overlay = None;
    if let Some((id, model, tuning, route)) = outcome {
        apply_model_switch(tui, ctx, &id, &model, &tuning, route.as_ref());
    }
}

/// Poll keys and landed fetches until the picker resolves; `None` on cancel.
/// The `route` is the `OpenRouter` serving-provider slug, `None` for auto.
fn drive_picker(
    tui: &mut Tui,
    store: &CredentialStore,
    catalog: &mut ModelCatalog<LiveSource>,
    mut listing: Listing,
) -> Option<(
    provider::ProviderId,
    String,
    provider::Tuning,
    Option<String>,
)> {
    // Spawned from inside the loop, unlike `listing`, whose fetches are all away
    // before it.
    let mut endpoints: Fetches<String, Vec<ProviderEndpoint>> = Fetches::new();
    loop {
        // The picker's copy is for render; `listing` stays authoritative.
        for id in listing.pump(catalog) {
            if let (Some(state), Some(p)) = (listing.state(&id), tui.app.picker_mut()) {
                p.set_models(&id, state.clone());
            }
        }
        for (model, result) in endpoints.landed() {
            let state = match result {
                Ok(list) => {
                    catalog.record_endpoints(&model, list.clone());
                    picker::EndpointsState::Loaded(list)
                }
                Err(reason) => picker::EndpointsState::Failed(reason),
            };
            if let Some(p) = tui.app.picker_mut() {
                p.set_endpoints(&model, state);
            }
        }
        // Seeding the state is also the dedup: the next poll no longer reports
        // this model as needing a fetch.
        let needed = tui
            .app
            .picker_mut()
            .and_then(|p| p.focused_or_model_needing_endpoints());
        if let Some(model) = needed {
            if let Some(list) = catalog.cached_endpoints(&model) {
                if let Some(p) = tui.app.picker_mut() {
                    p.set_endpoints(&model, picker::EndpointsState::Loaded(list));
                }
            } else {
                if let Some(p) = tui.app.picker_mut() {
                    p.set_endpoints(&model, picker::EndpointsState::Loading);
                }
                let source = catalog.source().clone();
                endpoints.spawn(model.clone(), move || source.endpoints(&model));
            }
        }
        match overlay_tick(tui) {
            OverlayTick::TerminalLost | OverlayTick::Cancel => return None,
            OverlayTick::Idle => {}
            OverlayTick::Key(code) => {
                let action = tui.app.picker_mut()?.key(code);
                match action {
                    picker::PickAction::None => {}
                    picker::PickAction::Selected(id, model, tuning, route) => {
                        return Some((id, model, tuning, route));
                    }
                    picker::PickAction::Manual(query, tuning) => {
                        let available = store.available();
                        match crate::provider::models::resolve_model_provider(
                            &query, &available, catalog,
                        ) {
                            Ok(id) => return Some((id, query, tuning, None)),
                            Err(e) => {
                                // The dialogue's own failure, not an action on an
                                // agent, so it lands on root and not the tab a
                                // switch addresses.
                                let root = tui.app.tabs.root();
                                tui.app.push_error(root, &e);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Rebuild the provider for `model` and swap it into the *focused* agent's
/// handle, which its next turn reads; a failed persist leaves that switch
/// standing. The note records as a [`Forensic::SystemNote`] beside its own
/// [`Forensic::ModelChanged`], so a real operational event reaches the trace
/// the same way a worker's does; its own failures are view chrome.
///
/// [`Forensic::SystemNote`]: crate::record::Forensic::SystemNote
/// [`Forensic::ModelChanged`]: crate::record::Forensic::ModelChanged
fn apply_model_switch(
    tui: &mut Tui,
    ctx: &CommandCtx<'_>,
    provider_id: &provider::ProviderId,
    model: &str,
    tuning: &provider::Tuning,
    route: Option<&String>,
) {
    let store = &*ctx.store;
    let info = ctx.info;
    let recorder = ctx.recorder;
    // Every failure below answers one gesture on this tab, so it lands here —
    // the persist too, whose file is project-wide but whose message is not.
    let focused = tui.app.tabs.focused();
    let Some(cred) = store.get(provider_id).cloned() else {
        tui.app.push_error(
            focused,
            &format!("{} has no resolved credential", provider_id.label()),
        );
        return;
    };
    // A tab that settled while the picker was open has no handle to swap.
    let Some(provider) = ctx.agents.provider(focused) else {
        tui.app
            .push_error(focused, "the focused agent is no longer live");
        return;
    };
    // The token override is no part of the selection, so it rides across by hand.
    let current_override = provider.current().max_tokens_override();
    let engine = ctx.engine.clone();
    let new_provider = Arc::new(Provider::build(
        engine,
        provider_id,
        model.to_string(),
        &cred,
        current_override,
        tuning.clone(),
        route.cloned(),
    ));
    let label = provider_id.label();
    provider.swap(new_provider);
    tui.app.update_live_model(&provider.current());
    let state_dir = crate::bootstrap::EXARCH.project_dir(info.cwd);
    if let Err(e) = state::save(
        &state_dir,
        &state::State::new(provider_id, model, tuning, route.map(String::as_str)),
    ) {
        tui.app
            .push_error(focused, &format!("could not persist selection: {e}"));
    }
    let text = format!(
        "[Switched to {label} {model}{}]",
        tuning_suffix(tuning, route.map(String::as_str))
    );
    if let Err(error) = recorder.emit(crate::record::Forensic::SystemNote { text }) {
        recorder.report_fault(&error);
    }
    if let Err(error) = recorder.emit(crate::record::Forensic::ModelChanged {
        model: model.to_string(),
        provider: label.to_string(),
    }) {
        recorder.report_fault(&error);
    }
}

/// The switch note's ` · effort high · temp 0.7 · via deepinfra` tail; empty
/// when every knob is auto and no route is pinned.
fn tuning_suffix(tuning: &provider::Tuning, route: Option<&str>) -> String {
    let mut parts = String::new();
    if let Some(effort) = &tuning.effort {
        let _ = write!(parts, " · effort {}", effort.variant_name());
    }
    if let Some(temperature) = tuning.temperature {
        let _ = write!(parts, " · temp {temperature:.1}");
    }
    if let Some(top_p) = tuning.top_p {
        let _ = write!(parts, " · top_p {top_p:.2}");
    }
    if let Some(slug) = route {
        let _ = write!(parts, " · via {slug}");
    }
    parts
}
