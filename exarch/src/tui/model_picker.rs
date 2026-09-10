//! The `/model` overlay's orchestration.
//!
//! [`Picker`] is display and input only; this module, its one caller, reaches
//! the [`Bureau`] holding the credentials, the model catalog, and the network
//! seam, and turns a resolved [`picker::PickAction`] into a live provider swap
//! plus a saved [`state::State`]. [`super::login`] mirrors the split.

use crate::provider::identity::{self, Account};
use crate::provider::listing::{Fetches, Listing};
use crate::provider::models::{ModelSource, ProviderEndpoint};
use crate::provider::state;
use crate::provider::{self, Bureau};

use super::app::Overlay;
use super::picker::{self, Picker};
use super::tui_loop::{CommandCtx, OverlayTick, Tui, overlay_tick};

pub(super) fn pick_model(tui: &mut Tui, ctx: &CommandCtx<'_>) {
    let bureau = ctx.bureau;
    let available = bureau.available();
    // Open on the focused agent's live tuning; a settled one falls back to the
    // defaults.
    let initial_tuning = tui
        .app
        .tabs
        .focused_agent()
        .map(|agent| agent.current_provider().tuning().clone())
        .unwrap_or_default();
    let mut picker = Picker::new(
        available.clone(),
        &initial_tuning,
        crate::provider::pricing::caps_or_default,
    );
    // `Picker::new` seeded every row `Loading`, so only the rows `Listing::open`
    // settled from cache need forwarding; the misses land as `drive_picker` pumps.
    let ids = available.iter().map(|account| account.id.clone()).collect();
    // A scripted session has no catalog to list from, so there is nothing to
    // pick between and no rows to seed.
    let Some(listing) = bureau.with_catalog(|catalog| Listing::open(ids, catalog)) else {
        return;
    };
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
    let outcome = drive_picker(tui, bureau, listing);
    tui.app.overlay = None;
    if let Some((account, model, tuning, route)) = outcome {
        apply_model_switch(tui, ctx, &account, &model, &tuning, route.as_ref());
    }
}

/// Poll keys and landed fetches until the picker resolves; `None` on cancel.
/// The `route` is the `OpenRouter` serving-provider slug, `None` for auto.
///
/// The catalog is taken per fold rather than held across the loop: the
/// listing's own fetches run on their own threads, so no frame holds a bureau
/// lock while one of them is out.
fn drive_picker(
    tui: &mut Tui,
    bureau: &Bureau,
    mut listing: Listing,
) -> Option<(Account, String, provider::Tuning, Option<String>)> {
    // Spawned from inside the loop, unlike `listing`, whose fetches are all away
    // before it.
    let mut endpoints: Fetches<String, Vec<ProviderEndpoint>> = Fetches::new();
    loop {
        // The picker's copy is for render; `listing` stays authoritative.
        let woken = bureau
            .with_catalog(|catalog| listing.pump(catalog))
            .unwrap_or_default();
        for id in woken {
            if let (Some(state), Some(p)) = (listing.state(&id), tui.app.picker_mut()) {
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
            let cached = bureau
                .with_catalog(|catalog| catalog.cached_endpoints(&model))
                .flatten();
            if let Some(list) = cached {
                if let Some(p) = tui.app.picker_mut() {
                    p.set_endpoints(&model, picker::EndpointsState::Loaded(list));
                }
            } else {
                if let Some(p) = tui.app.picker_mut() {
                    p.set_endpoints(&model, picker::EndpointsState::Loading);
                }
                // The seam is cloned out and the fetch runs on its own thread,
                // so the catalog is free again before the network is touched.
                if let Some(source) = bureau.with_catalog(|catalog| catalog.source().clone()) {
                    endpoints.spawn(model.clone(), move || source.endpoints(&model));
                }
            }
        }
        match overlay_tick(tui) {
            OverlayTick::TerminalLost | OverlayTick::Cancel => return None,
            OverlayTick::Idle => {}
            OverlayTick::Key(code) => {
                let action = tui.app.picker_mut()?.key(code);
                match action {
                    picker::PickAction::None => {}
                    picker::PickAction::Selected(account, model, tuning, route) => {
                        return Some((account, model, tuning, route));
                    }
                    picker::PickAction::Manual(query, tuning) => {
                        let available = bureau.available();
                        // The one fold that may fetch under the lock: a typed
                        // name must be attributed before the overlay can close,
                        // and nothing else takes the catalog while it is up.
                        let resolved = bureau.with_catalog(|catalog| {
                            crate::provider::models::resolve_model_provider(
                                &query, &available, catalog,
                            )
                        })?;
                        match resolved {
                            Ok(account) => return Some((account, query, tuning, None)),
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
/// standing. The switch reaches the trace as a [`Forensic::ModelChanged`] and
/// the screen as the status bar's live label — never as transcript chatter;
/// its own failures are view chrome.
///
/// [`Forensic::ModelChanged`]: crate::record::Forensic::ModelChanged
fn apply_model_switch(
    tui: &mut Tui,
    ctx: &CommandCtx<'_>,
    account: &Account,
    model: &str,
    tuning: &provider::Tuning,
    route: Option<&String>,
) {
    let info = ctx.info;
    let recorder = ctx.recorder;
    // Every failure below answers one gesture on this tab, so it lands here —
    // the persist too, whose file is project-wide but whose message is not.
    let focused = tui.app.tabs.focused();
    let available = ctx.bureau.available();
    let label = identity::label(account, &available);
    // A tab that settled while the picker was open has no handle to swap.
    let Some(agent) = tui.app.tabs.agent(focused) else {
        tui.app
            .push_error(focused, "the focused agent is no longer live");
        return;
    };
    let provider = agent.provider_handle();
    // The token override is no part of the selection, so it rides across by hand.
    let current_override = provider.current().max_tokens_override();
    let new_provider = match ctx.bureau.build(
        account,
        model.to_string(),
        tuning,
        route.cloned(),
        current_override,
    ) {
        Ok(built) => built,
        Err(e) => {
            tui.app.push_error(focused, &e);
            return;
        }
    };
    provider.swap(new_provider);
    tui.app.update_live_model(&provider.current(), &available);
    let state_dir = crate::bootstrap::EXARCH.project_dir(info.cwd);
    if let Err(e) = state::save(
        &state_dir,
        &state::State::new(
            account,
            &available,
            model,
            tuning,
            route.map(String::as_str),
        ),
    ) {
        tui.app
            .push_error(focused, &format!("could not persist selection: {e}"));
    }
    if let Err(error) = recorder.emit(crate::record::Forensic::ModelChanged {
        model: model.to_string(),
        label,
        service: Some(account.service.name.as_str().to_string()),
        account: Some(account.id.as_str().to_string()),
    }) {
        recorder.report_fault(&error);
    }
}
