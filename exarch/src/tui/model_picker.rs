//! Model picker overlay orchestration — the `/model` command handler and its
//! event loop.

use std::fmt::Write;
use std::sync::Arc;

use crate::bus::Kind;
use crate::provider::credential::CredentialStore;
use crate::provider::listing::{Fetches, Listing};
use crate::provider::models::{LiveSource, ModelCatalog, ModelSource, ProviderEndpoint};
use crate::provider::state;
use crate::provider::{self, Provider};

use super::app::Overlay;
use super::picker::{self, Picker};
use super::tui_loop::{CommandCtx, OverlayTick, Tui, overlay_tick};

pub(super) fn pick_model(tui: &mut Tui, ctx: &mut CommandCtx<'_>) {
    // A shared reborrow: `pick_model` only reads the store (`/login`'s
    // `add_oauth` is the one site that needs it mutably), so `ctx` stays
    // whole for `apply_model_switch`'s later reborrow.
    let store = &*ctx.store;
    let available = store.available();
    // Each plan-backed provider's flavour, for the picker's labels: a ChatGPT
    // login (the OAuth credential) reads as the ChatGPT plan, an otherwise-
    // metered provider whose `ProviderId` declares a flat rate (opencode Go) as
    // the generic subscription. A provider absent from the map is metered.
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
    // Seed the tuning controls from the focused provider's live values, so the
    // overlay opens showing the effort/temperature currently in force (a
    // settled agent with no live handle falls back to the defaults).
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
    // Open every available provider's model listing: a provider already
    // cached in the catalog seeds `Loaded` with no network touched; a miss
    // seeds `Loading` and spawns its background fetch through the catalog's
    // `ModelSource`, so the overlay opens instantly and the misses fill in as
    // `drive_picker` pumps them. `Picker::new` already seeded every row
    // `Loading`, so only the already-settled rows need forwarding here.
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

/// Poll keys and background-fetch results until the picker resolves.  Returns
/// the chosen `(provider, model, tuning, route)`, or `None` on cancel. The
/// `route` is the chosen `OpenRouter` serving-provider slug (`None` for auto).
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
    // The serving-provider fetch is intent-driven and spawned from inside the
    // loop, so this pump lives for the loop's whole duration (unlike
    // `listing`, whose fetches are all kicked off before the loop by
    // `pick_model`).
    let mut endpoints: Fetches<String, Vec<ProviderEndpoint>> = Fetches::new();
    loop {
        // Fold any landed model-list results into the picker; `Listing::pump`
        // itself records each success into the catalog, on this thread, so
        // the disk write stays single-threaded.
        for id in listing.pump(catalog) {
            if let (Some(state), Some(p)) = (listing.state(&id), tui.app.picker_mut()) {
                p.set_models(&id, cloned_state(state));
            }
        }
        // Fold any landed serving-provider results the same way.
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
        // When the provider control is focused on an OpenRouter model whose
        // serving providers we have not fetched, seed it from the catalog memo
        // or spawn a background fetch. Seeding the state first dedups: the next
        // poll no longer reports the model as needing a fetch.
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

/// Clone a fetch state's payload into a fresh state for the picker's own
/// view — [`Listing`] and [`Fetches`] hold the authoritative copy (behind the
/// catalog, or in flight on a worker thread); the picker keeps this one for
/// rendering, exactly as [`Picker::set_models`](picker::Picker::set_models)
/// and [`Picker::set_endpoints`](picker::Picker::set_endpoints) expect.
fn cloned_state<T: Clone>(state: &picker::FetchState<T>) -> picker::FetchState<T> {
    match state {
        picker::FetchState::Loading => picker::FetchState::Loading,
        picker::FetchState::Loaded(value) => picker::FetchState::Loaded(value.clone()),
        picker::FetchState::Failed(reason) => picker::FetchState::Failed(reason.clone()),
    }
}

/// Rebuild the provider for the chosen `kind` + `model` over the same
/// transcript and swap it into the **focused agent's** own provider handle
/// (its next turn reads it), persist the selection to the project state dir,
/// and update the live status bar. A persistence failure is noted but does not
/// undo the in-memory switch.  A focused agent that settled between the picker
/// opening and the selection has no handle to swap, so the switch is dropped.
///
/// A model switch is a *real* operational event, so it goes through `emit` —
/// the UI-thread recording emitter carrying the trunk's transcript — as a
/// [`Kind::SystemNote`].  It records in the trace and draws through the normal
/// bus path, like a worker-raised note; the UI never fabricates an `Event` for
/// it.  Its own failures, by contrast, are view chrome ([`App::push_error`]).
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
    let emit = ctx.emit;
    let id = tui.app.tabs.root();
    let Some(cred) = store.get(provider_id).cloned() else {
        tui.app.push_error(
            id,
            &format!("{} has no resolved credential", provider_id.label()),
        );
        return;
    };
    // Swap the *focused* agent's handle; if it has settled, there is nothing to
    // swap and the selection is dropped (the user can reopen on a live tab).
    let focused = tui.app.tabs.focused();
    let Some(provider) = ctx.agents.provider(focused) else {
        tui.app
            .push_error(id, "the focused agent is no longer live");
        return;
    };
    // Capture the current provider's max_tokens_override before we swap,
    // so the new provider carries the same user override.
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
            .push_error(id, &format!("could not persist selection: {e}"));
    }
    emit.emit(Kind::SystemNote(format!(
        "[Switched to {label} {model}{}]",
        tuning_suffix(tuning, route.map(String::as_str))
    )));
}

/// A human-readable suffix for the switch note describing any non-default
/// tuning and route, e.g. ` · effort high · temp 0.7 · top_p 0.9 · via
/// deepinfra`. Empty when every knob is auto and no route is pinned.
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
