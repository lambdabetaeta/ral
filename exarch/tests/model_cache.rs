#![allow(clippy::disallowed_methods)]

//! [`ModelCatalog`]'s on-disk half, in its own test binary.
//!
//! The cache lives under `$XDG_CACHE_HOME`, which the scenario must set for
//! real; as in `credential_env.rs`, the process is the isolation boundary, so
//! no bystander test shares the mutated environment. One scenario, because the
//! stages share one file: fetch, serve from disk, upsert, expire.

use exarch::bootstrap::App;
use exarch::provider::identity::{AccountId, ServiceName, built_in};
use exarch::provider::models::{ModelCatalog, ModelSource, ProviderEndpoint};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A key-bearing built-in service's account id is its own name.
fn account_id(name: &str) -> AccountId {
    let service = built_in(&ServiceName::declared(name).unwrap()).unwrap();
    AccountId::of_service(&service.name)
}

/// One list for every account, counting the fetches no cache could absorb.
struct FakeSource {
    fetches: Arc<AtomicUsize>,
}

impl ModelSource for FakeSource {
    fn list(&self, _account: &AccountId) -> Result<Vec<String>, String> {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        Ok(vec!["claude-opus-4".into(), "claude-haiku-4".into()])
    }

    fn endpoints(&self, _model: &str) -> Result<Vec<ProviderEndpoint>, String> {
        Err("this scenario never routes".into())
    }
}

fn read_cache(path: &Path) -> serde_json::Value {
    let bytes = std::fs::read(path).expect("the catalog wrote its cache");
    serde_json::from_slice(&bytes).expect("the cache is JSON")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs()
}

#[test]
fn disk_cache_serves_a_fresh_entry_and_refetches_a_stale_one() {
    let dir = std::env::temp_dir().join(format!("exarch-model-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };
    let path = dir.join("exarch").join("models.json");

    let fetches = Arc::new(AtomicUsize::new(0));
    let catalog = || {
        ModelCatalog::new(
            FakeSource {
                fetches: Arc::clone(&fetches),
            },
            App::new("exarch"),
        )
    };
    let anthropic = account_id("anthropic");
    let models = vec!["claude-opus-4".to_string(), "claude-haiku-4".to_string()];

    // A cold catalog fetches once, and persists what it got.
    assert_eq!(catalog().list(&anthropic), Some(models.clone()));
    assert_eq!(fetches.load(Ordering::Relaxed), 1);

    // The next session's memo is empty, so a hit here is the file's doing.
    let mut session = catalog();
    assert_eq!(session.cached(&anthropic), Some(models.clone()));
    assert_eq!(fetches.load(Ordering::Relaxed), 1);

    // A second account lands beside the first rather than over it.
    session.record(&account_id("deepseek"), vec!["deepseek-chat".into()]);
    let mut file = read_cache(&path);
    assert_eq!(
        file["providers"]["anthropic"]["models"],
        serde_json::json!(models)
    );
    assert_eq!(
        file["providers"]["deepseek"]["models"],
        serde_json::json!(["deepseek-chat"])
    );

    // Aged past the 24-hour TTL, the entry stops being served and is refetched.
    let stale = now_secs() - 25 * 3600;
    file["providers"]["anthropic"]["fetched_at"] = stale.into();
    std::fs::write(&path, file.to_string()).expect("rewrite the cache");

    let mut later = catalog();
    assert_eq!(later.cached(&anthropic), None);
    assert_eq!(later.list(&anthropic), Some(models));
    assert_eq!(fetches.load(Ordering::Relaxed), 2);
    assert!(
        read_cache(&path)["providers"]["anthropic"]["fetched_at"]
            .as_u64()
            .is_some_and(|stamp| stamp > stale),
        "a refetch must carry the stamp forward"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
