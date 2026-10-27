//! The live frame's network seal: three private `WebKit` settings that CSP does not reach.

use std::{cell::Cell, panic::AssertUnwindSafe};

use block2::RcBlock;
use objc2::{MainThreadMarker, exception, rc::Retained, runtime::AnyObject};
use objc2_foundation::{
    NSError, NSNumber, NSObject, NSObjectNSKeyValueCoding as _, NSSet, NSString,
};
use objc2_web_kit::{
    WKContentRuleList, WKContentRuleListStore, WKPreferences, WKWebViewConfiguration,
};

/// Proof that the webview refuses all network egress; no live page is served without it.
pub(crate) struct Sealed(());

const HOSTS: &str = "allowedNetworkHosts";
const PEER_CONNECTION: &str = "peerConnectionEnabled";
const RULES_ID: &str = "synod-live-seal";
const RULES: &str = r#"[{"trigger":{"url-filter":"^https?:"},"action":{"type":"block"}},{"trigger":{"url-filter":"^wss?:"},"action":{"type":"block"}}]"#;

/// A configuration to create the webview from. Fail closed: the keys are private
/// `WebKit` API that an OS update may drop, so the seal is proved by read-back, not assumed.
pub(crate) fn configuration(
    on_sealed: impl FnOnce(Result<Sealed, String>) + 'static,
) -> Retained<WKWebViewConfiguration> {
    let mtm = MainThreadMarker::new().expect("a webview configuration is made on the main thread");
    // SAFETY: called with the main-thread marker.
    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    match apply(&config) {
        Ok(()) => compile_rules(mtm, &config, on_sealed),
        Err(why) => on_sealed(Err(why)),
    }
    config
}

fn preferences(config: &WKWebViewConfiguration) -> Retained<WKPreferences> {
    // SAFETY: a plain accessor.
    unsafe { config.preferences() }
}

fn apply(config: &WKWebViewConfiguration) -> Result<(), String> {
    set(config, HOSTS, &NSSet::<AnyObject>::new())?;
    set(
        &preferences(config),
        PEER_CONNECTION,
        &NSNumber::new_bool(false),
    )
}

fn confirm(config: &WKWebViewConfiguration) -> Result<(), String> {
    let hosts = read(config, HOSTS)?;
    require(
        HOSTS,
        hosts
            .downcast_ref::<NSSet<AnyObject>>()
            .is_some_and(NSSet::is_empty),
        "the empty set",
        &hosts,
    )?;
    let peer = read(&preferences(config), PEER_CONNECTION)?;
    require(
        PEER_CONNECTION,
        peer.downcast_ref::<NSNumber>()
            .is_some_and(|n| !n.as_bool()),
        "NO",
        &peer,
    )
}

/// Compiles the rule list, adds it, and only then confirms every setting and reports.
fn compile_rules(
    mtm: MainThreadMarker,
    config: &Retained<WKWebViewConfiguration>,
    on_sealed: impl FnOnce(Result<Sealed, String>) + 'static,
) {
    // SAFETY: called with the main-thread marker.
    let Some(store) = (unsafe { WKContentRuleListStore::defaultStore(mtm) }) else {
        return on_sealed(Err(failure(
            "WebKit has no default content rule list store",
        )));
    };
    let on_sealed = Cell::new(Some(on_sealed));
    let handler = RcBlock::new({
        let config = config.clone();
        move |list: *mut WKContentRuleList, error: *mut NSError| {
            let sealed = install(&config, list, error)
                .and_then(|()| confirm(&config))
                .map(|()| Sealed(()));
            if let Some(on_sealed) = on_sealed.take() {
                on_sealed(sealed);
            }
        }
    });
    // SAFETY: both strings are non-nil, and WebKit copies the handler block.
    unsafe {
        store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(
            Some(&NSString::from_str(RULES_ID)),
            Some(&NSString::from_str(RULES)),
            Some(&handler),
        );
    }
}

fn install(
    config: &WKWebViewConfiguration,
    list: *mut WKContentRuleList,
    error: *mut NSError,
) -> Result<(), String> {
    // SAFETY: WebKit passes each pointer null or live for the whole call.
    if let Some(list) = unsafe { list.as_ref() } {
        // SAFETY: the user content controller is shared with the live webview.
        unsafe { config.userContentController().addContentRuleList(list) };
        return Ok(());
    }
    // SAFETY: as above.
    let cause = unsafe { error.as_ref() }.map_or_else(
        || "no error was given".to_owned(),
        |error| error.localizedDescription().to_string(),
    );
    Err(failure(format_args!(
        "WebKit could not compile the content rule list: {cause}"
    )))
}

fn set(object: &NSObject, key: &str, value: &AnyObject) -> Result<(), String> {
    // SAFETY: `value` is the type WebKit's setter for `key` takes.
    guarded(key, "apply", || unsafe {
        object.setValue_forKey(Some(value), &NSString::from_str(key));
    })
}

fn read(object: &NSObject, key: &str) -> Result<Retained<AnyObject>, String> {
    guarded(key, "read back", || {
        object.valueForKey(&NSString::from_str(key))
    })?
    .ok_or_else(|| failure(format_args!("WebKit setting `{key}` reads back as nil")))
}

fn require(key: &str, holds: bool, expected: &str, found: &AnyObject) -> Result<(), String> {
    if holds {
        Ok(())
    } else {
        Err(failure(format_args!(
            "WebKit setting `{key}` reads back as {found:?}, not {expected}"
        )))
    }
}

/// An undefined key raises an `NSException`, which must not unwind through Rust.
fn guarded<R>(key: &str, doing: &str, f: impl FnOnce() -> R) -> Result<R, String> {
    exception::catch(AssertUnwindSafe(f)).map_err(|raised| {
        let cause = raised.map_or_else(|| "an unknown exception".to_owned(), |e| e.to_string());
        let cause = cause.trim_end_matches('.');
        failure(format_args!(
            "WebKit could not {doing} the setting `{key}`: {cause}"
        ))
    })
}

fn failure(what: impl std::fmt::Display) -> String {
    format!(
        "synod-live seal: {what}; live pages stay off until this is fixed (the seal uses private WebKit settings that a macOS update can change)"
    )
}

#[cfg(test)]
mod tests {
    use super::RULES;

    #[test]
    fn the_rule_list_blocks_web_and_socket_schemes() {
        let rules: serde_json::Value = serde_json::from_str(RULES).expect("RULES is JSON");
        let rules = rules.as_array().expect("RULES is an array");
        assert_eq!(rules.len(), 2);
        assert!(rules.iter().all(|rule| rule["action"]["type"] == "block"));
    }
}
