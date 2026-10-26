//! Credential ownership and the shared store's transaction boundary.

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;

use super::{OAuthToken, account_id, read_at, refresh, write_all_at};
use crate::provider::identity::AccountId;
use ral_core::own::OwnDir;
use ral_core::sync::LockExt;
use std::collections::BTreeMap;
use std::fs::{File, TryLockError};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::time::{Instant, sleep, timeout_at};

const DEADLINE: Duration = Duration::from_secs(30);
static EXECUTOR: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build OAuth runtime")
});
static GATE: AsyncMutex<()> = AsyncMutex::const_new(());
type Registry = BTreeMap<(PathBuf, AccountId), Arc<Owned>>;
// Ambiguous and unsaved outcomes outlive their last consumer.
static HANDLES: Mutex<Registry> = Mutex::new(BTreeMap::new());
type Completion = watch::Receiver<Option<Result<(), String>>>;

struct Owned {
    store: OwnDir,
    id: AccountId,
    state: Mutex<State>,
}

struct State {
    credential: CredentialState,
    pending: Option<Completion>,
}

#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
enum CredentialState {
    Ready(OAuthToken),
    Uncertain(OAuthToken),
    Unsaved {
        predecessor: OAuthToken,
        successor: OAuthToken,
    },
    Retired(OAuthToken),
}

impl CredentialState {
    fn token(&self) -> &OAuthToken {
        match self {
            Self::Ready(token) | Self::Uncertain(token) | Self::Retired(token) => token,
            Self::Unsaved { successor, .. } => successor,
        }
    }

    fn reconcile(&self, disk: &OAuthToken) -> Self {
        match self {
            Self::Unsaved { predecessor, .. } if predecessor == disk => self.clone(),
            Self::Ready(token) | Self::Uncertain(token) if token == disk => self.clone(),
            Self::Ready(_) | Self::Uncertain(_) | Self::Unsaved { .. } | Self::Retired(_) => {
                Self::Ready(disk.clone())
            }
        }
    }
}

/// One service-qualified account; snapshots never wait for network or disk.
#[derive(Clone)]
pub struct CredentialHandle(Arc<Owned>);

impl std::fmt::Debug for CredentialHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialHandle").finish_non_exhaustive()
    }
}

impl CredentialHandle {
    #[cfg(any(test, feature = "test-util"))]
    pub fn shares_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn snapshot(&self) -> OAuthToken {
        self.0.state.lock_ignore_poison().credential.token().clone()
    }

    pub fn is_retired(&self) -> bool {
        matches!(
            self.0.state.lock_ignore_poison().credential,
            CredentialState::Retired(_)
        )
    }

    /// Dropping a waiter cannot cancel a submitted rotating-token exchange.
    ///
    /// # Errors
    /// Removal, an unresolved rotation, or a store/issuer deadline or failure.
    pub async fn prepare(&self) -> Result<(), String> {
        self.prepare_until(Instant::now() + DEADLINE, async |current| {
            refresh(&current).await
        })
        .await
    }

    async fn prepare_until<F, Fut>(&self, deadline: Instant, exchange: F) -> Result<(), String>
    where
        F: FnOnce(OAuthToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<OAuthToken, String>> + Send,
    {
        let mut result = {
            let mut state = self.0.state.lock_ignore_poison();
            if let Some(pending) = &state.pending {
                pending.clone()
            } else {
                let (publish, result) = watch::channel(None);
                state.pending = Some(result.clone());
                drop(state);
                let owner = self.clone();
                let task = owner.clone();
                let rotation = EXECUTOR.spawn(async move { task.rotate(deadline, exchange).await });
                EXECUTOR.spawn(async move {
                    let settled = rotation
                        .await
                        .unwrap_or_else(|_| Err(owner.recovery("refresh owner panicked")));
                    let mut state = owner.0.state.lock_ignore_poison();
                    publish.send_replace(Some(settled));
                    state.pending = None;
                });
                result
            }
        };
        loop {
            let settled = result.borrow_and_update().clone();
            if let Some(settled) = settled {
                return settled;
            }
            result
                .changed()
                .await
                .map_err(|_| "OAuth refresh owner stopped".to_string())?;
        }
    }

    fn recovery(&self, reason: &str) -> String {
        format!(
            "OAuth account {:?} {reason}; sign in again to that service using that account",
            self.0.id.as_str()
        )
    }

    async fn rotate<F, Fut>(&self, deadline: Instant, exchange: F) -> Result<(), String>
    where
        F: FnOnce(OAuthToken) -> Fut,
        Fut: Future<Output = Result<OAuthToken, String>>,
    {
        let lease = lease(&self.0.store, deadline).await?;
        let (host, mut tokens) = read_at(&lease.store)?;
        let Some(index) = tokens
            .iter()
            .position(|token| account_id(token) == self.0.id)
        else {
            lease.retire(&self.snapshot());
            return Err(self.recovery("has been removed"));
        };
        let credential = {
            let mut state = self.0.state.lock_ignore_poison();
            state.credential = state.credential.reconcile(&tokens[index]);
            state.credential.clone()
        };
        let current = match credential {
            CredentialState::Retired(_) => return Err(self.recovery("has been signed out")),
            CredentialState::Uncertain(_) => {
                return Err(self.recovery("has an unresolved refresh outcome"));
            }
            CredentialState::Unsaved { successor, .. } => {
                tokens[index] = successor.clone();
                write_all_at(&lease.store, host.as_deref(), &tokens).map_err(|error| {
                    self.recovery(&format!(
                        "could not persist its rotated credential: {error}"
                    ))
                })?;
                lease.publish(&successor);
                successor
            }
            CredentialState::Ready(token) => token,
        };
        if !current.is_stale() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("OAuth preparation deadline exceeded before refresh".into());
        }
        self.0.state.lock_ignore_poison().credential = CredentialState::Uncertain(current.clone());
        let fresh = match timeout_at(deadline, exchange(current.clone())).await {
            Ok(Ok(fresh)) => fresh,
            result => {
                let reason = match result {
                    Ok(Err(error)) => format!("could not refresh: {error}"),
                    Err(_) => {
                        "refresh timed out; the issuer may have rotated its credential".into()
                    }
                    Ok(Ok(_)) => unreachable!(),
                };
                return Err(self.recovery(&reason));
            }
        };
        self.0.state.lock_ignore_poison().credential = CredentialState::Unsaved {
            predecessor: current,
            successor: fresh.clone(),
        };
        tokens[index] = fresh.clone();
        write_all_at(&lease.store, host.as_deref(), &tokens).map_err(|error| {
            self.recovery(&format!(
                "rotated but could not save its credential: {error}"
            ))
        })?;
        lease.publish(&fresh);
        drop(lease);
        Ok(())
    }
}

/// All credential publishers hold this lease through durable commit.
pub(super) struct StoreLease {
    store: OwnDir,
    _file: File,
    _gate: tokio::sync::MutexGuard<'static, ()>,
}

impl StoreLease {
    fn attach(&self, token: &OAuthToken) -> CredentialHandle {
        let key = (self.store.path().to_path_buf(), account_id(token));
        let mut handles = HANDLES.lock_ignore_poison();
        let owner = handles.entry(key.clone()).or_insert_with(|| {
            Arc::new(Owned {
                store: self.store.clone(),
                id: key.1,
                state: Mutex::new(State {
                    credential: CredentialState::Ready(token.clone()),
                    pending: None,
                }),
            })
        });
        let handle = CredentialHandle(owner.clone());
        drop(handles);
        handle
    }

    pub(super) fn credentials(&self) -> Result<Vec<CredentialHandle>, String> {
        let (_, tokens) = read_at(&self.store)?;
        Ok(tokens
            .iter()
            .map(|token| {
                let handle = self.attach(token);
                {
                    let mut state = handle.0.state.lock_ignore_poison();
                    state.credential = state.credential.reconcile(token);
                }
                handle
            })
            .collect())
    }

    fn publish(&self, token: &OAuthToken) -> CredentialHandle {
        let handle = self.attach(token);
        handle.0.state.lock_ignore_poison().credential = CredentialState::Ready(token.clone());
        handle
    }

    fn retire(&self, token: &OAuthToken) {
        let key = (self.store.path().to_path_buf(), account_id(token));
        if let Some(owner) = HANDLES.lock_ignore_poison().get(&key) {
            owner.state.lock_ignore_poison().credential = CredentialState::Retired(token.clone());
        }
    }

    pub(super) fn commit_login(
        &self,
        token: &OAuthToken,
    ) -> Result<(CredentialHandle, bool), String> {
        let (host, mut tokens) = read_at(&self.store)?;
        let id = account_id(token);
        let replaced =
            if let Some(existing) = tokens.iter_mut().find(|entry| account_id(entry) == id) {
                *existing = token.clone();
                true
            } else {
                tokens.push(token.clone());
                false
            };
        write_all_at(&self.store, host.as_deref(), &tokens)?;
        Ok((self.publish(token), replaced))
    }

    pub(super) fn host_id(&self) -> Result<String, String> {
        let (host, tokens) = read_at(&self.store)?;
        if let Some(host) = host {
            return Ok(host);
        }
        let host = uuid::Uuid::new_v4().urn().to_string();
        write_all_at(&self.store, Some(&host), &tokens)?;
        Ok(host)
    }

    pub(super) fn remove(&self, account: &str) -> Result<Option<OAuthToken>, String> {
        let (host, mut tokens) = read_at(&self.store)?;
        let matched = super::matching_indices(&tokens, account, None);
        let [pos] = matched[..] else {
            return if matched.is_empty() {
                Ok(None)
            } else {
                Err(super::ambiguous(
                    account,
                    "log out",
                    &matched.iter().map(|&i| &tokens[i]).collect::<Vec<_>>(),
                ))
            };
        };
        let removed = tokens.remove(pos);
        if tokens.is_empty() {
            self.store.remove(super::TOKEN_FILE)?;
        } else {
            write_all_at(&self.store, host.as_deref(), &tokens)?;
        }
        self.retire(&removed);
        Ok(Some(removed))
    }

    pub(super) fn clear(&self) -> Result<Vec<OAuthToken>, String> {
        let (_, tokens) = read_at(&self.store)?;
        self.store.remove(super::TOKEN_FILE)?;
        for token in &tokens {
            self.retire(token);
        }
        Ok(tokens)
    }
}

async fn lease(store: &OwnDir, deadline: Instant) -> Result<StoreLease, String> {
    timeout_at(deadline, async {
        let gate = GATE.lock().await;
        // Never replace or unlink this rendezvous file.
        let file = store.open_write("oauth.lock")?;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(StoreLease {
                        store: store.clone(),
                        _gate: gate,
                        _file: file,
                    });
                }
                Err(TryLockError::WouldBlock) => sleep(Duration::from_millis(25)).await,
                Err(TryLockError::Error(error)) => {
                    return Err(format!("could not lock OAuth store: {error}"));
                }
            }
        }
    })
    .await
    .map_err(|_| "OAuth store transaction deadline exceeded".to_string())?
}

/// Synchronous callers are startup or background jobs, never a UI frame.
pub(super) fn transaction<R: Send + 'static>(
    store: &OwnDir,
    change: impl FnOnce(&StoreLease) -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    let store = store.clone();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    EXECUTOR.spawn(async move {
        let result = async {
            let lease = lease(&store, Instant::now() + DEADLINE).await?;
            change(&lease)
        }
        .await;
        let _ = send.send(result);
    });
    receive
        .recv()
        .map_err(|_| "OAuth transaction owner stopped".to_string())?
}
