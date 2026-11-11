//! Credential ownership and the shared store's transaction boundary.

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;

use super::{OAuthToken, RefreshError, account_id, read_at, refresh, write_all_at};
use crate::provider::identity::AccountId;
use ral_core::fact::{
    CredentialCause, CredentialCauseRefresh, CredentialCauseStore, CredentialFailure,
    CredentialService, CredentialStoreOperation, DurationFact, RefreshFailure, RefreshOutcome,
};
use ral_core::own::OwnDir;
use ral_core::sync::LockExt;
use std::collections::BTreeMap;
use std::fs::{File, TryLockError};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, oneshot, watch};
use tokio::time::{Instant, sleep, timeout, timeout_at};

/// Bounds waiting for the store lease and the decision to submit.
const DEADLINE: Duration = Duration::from_secs(30);
/// A submitted exchange has this budget, whatever the lease wait consumed.
const EXCHANGE_BUDGET: Duration = Duration::from_secs(30);
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
type Completion = watch::Receiver<Option<Result<(), CredentialFailure>>>;

struct Owned {
    store: OwnDir,
    id: AccountId,
    state: Mutex<State>,
}

struct State {
    credential: CredentialState,
    pending: Option<Completion>,
}

/// A credential in service; its token is the one to present.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
enum Live {
    Ready(OAuthToken),
    Uncertain(OAuthToken),
    Unsaved {
        predecessor: OAuthToken,
        successor: OAuthToken,
    },
}

impl Live {
    fn token(&self) -> &OAuthToken {
        match self {
            Self::Ready(token) | Self::Uncertain(token) => token,
            Self::Unsaved { successor, .. } => successor,
        }
    }
}

#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
enum CredentialState {
    Live(Live),
    Retired(OAuthToken),
}

impl From<Live> for CredentialState {
    fn from(live: Live) -> Self {
        Self::Live(live)
    }
}

impl CredentialState {
    fn token(&self) -> &OAuthToken {
        match self {
            Self::Live(live) => live.token(),
            Self::Retired(token) => token,
        }
    }

    /// Never `Retired`: a stored account is live again.
    fn reconcile(&self, disk: &OAuthToken) -> Live {
        match self {
            Self::Live(live @ Live::Unsaved { predecessor, .. }) if predecessor == disk => {
                live.clone()
            }
            Self::Live(live @ (Live::Ready(token) | Live::Uncertain(token))) if token == disk => {
                live.clone()
            }
            Self::Live(_) | Self::Retired(_) => Live::Ready(disk.clone()),
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
    /// Removal, an unresolved rotation, a lease deadline, or an issuer failure.
    pub async fn prepare(&self) -> Result<(), CredentialFailure> {
        self.prepare_until(Instant::now() + DEADLINE, async |current| {
            refresh(&current).await
        })
        .await
    }

    /// `deadline` bounds the lease and the decision to submit, not the exchange.
    async fn prepare_until<F, Fut>(
        &self,
        deadline: Instant,
        exchange: F,
    ) -> Result<(), CredentialFailure>
    where
        F: FnOnce(OAuthToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<OAuthToken, RefreshError>> + Send,
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
                        .unwrap_or_else(|_| Err(owner.failure(CredentialCause::OwnerPanicked)));
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
                .map_err(|_| self.failure(CredentialCause::OwnerStopped))?;
        }
    }

    fn failure(&self, cause: CredentialCause) -> CredentialFailure {
        let login = self.0.state.lock_ignore_poison().credential.token().login();
        CredentialFailure {
            account: self.0.id.as_str().to_owned(),
            service: match login {
                super::Login::Chatgpt => CredentialService::Chatgpt,
                super::Login::Codex => CredentialService::Codex,
                super::Login::Grok => CredentialService::Grok,
            },
            cause,
        }
    }

    fn store_failure(
        &self,
        operation: CredentialStoreOperation,
        error: &ral_core::own::OwnError,
    ) -> CredentialFailure {
        self.failure(store_cause(operation, error.into()))
    }

    fn set(&self, live: Live) {
        self.0.state.lock_ignore_poison().credential = live.into();
    }

    /// Adopt the stored token unless this owner already tracks it.
    fn reconcile(&self, disk: &OAuthToken) -> Live {
        let mut state = self.0.state.lock_ignore_poison();
        let live = state.credential.reconcile(disk);
        state.credential = live.clone().into();
        live
    }

    async fn rotate<F, Fut>(&self, deadline: Instant, exchange: F) -> Result<(), CredentialFailure>
    where
        F: FnOnce(OAuthToken) -> Fut,
        Fut: Future<Output = Result<OAuthToken, RefreshError>>,
    {
        let lease = lease(&self.0.store, deadline)
            .await
            .map_err(|cause| self.failure(cause))?;
        let (host, mut tokens) = read_at(&lease.store)
            .map_err(|error| self.store_failure(CredentialStoreOperation::Read, &error))?;
        let Some(index) = tokens
            .iter()
            .position(|token| account_id(token) == self.0.id)
        else {
            lease.retire(&self.snapshot());
            return Err(self.failure(CredentialCause::Removed));
        };
        let current = match self.reconcile(&tokens[index]) {
            Live::Uncertain(_) => {
                return Err(self.failure(CredentialCause::UnresolvedRotation));
            }
            Live::Unsaved { successor, .. } => {
                tokens[index] = successor.clone();
                write_all_at(&lease.store, host.as_deref(), &tokens).map_err(|error| {
                    self.store_failure(CredentialStoreOperation::PersistSuccessor, &error)
                })?;
                lease.publish(&successor);
                successor
            }
            Live::Ready(token) => token,
        };
        if !current.is_stale() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(self.failure(CredentialCause::PreparationDeadline));
        }
        let fresh = self.submit(current.clone(), exchange).await?;
        self.set(Live::Unsaved {
            predecessor: current,
            successor: fresh.clone(),
        });
        tokens[index] = fresh.clone();
        write_all_at(&lease.store, host.as_deref(), &tokens).map_err(|error| {
            self.store_failure(CredentialStoreOperation::PersistFreshRotation, &error)
        })?;
        lease.publish(&fresh);
        drop(lease);
        Ok(())
    }

    /// Only a failure that provably did not rotate `current` leaves it `Ready`.
    async fn submit<F, Fut>(
        &self,
        current: OAuthToken,
        exchange: F,
    ) -> Result<OAuthToken, CredentialFailure>
    where
        F: FnOnce(OAuthToken) -> Fut,
        Fut: Future<Output = Result<OAuthToken, RefreshError>>,
    {
        // `Uncertain` while in flight, so a panic cannot leave a replayable token.
        self.set(Live::Uncertain(current.clone()));
        let failure = match timeout(EXCHANGE_BUDGET, exchange(current.clone())).await {
            Ok(Ok(fresh)) => return Ok(fresh),
            Ok(Err(error)) => error,
            Err(_) => RefreshError::Ambiguous(RefreshFailure::TimedOut(DurationFact {
                seconds: EXCHANGE_BUDGET.as_secs().into(),
                nanos: EXCHANGE_BUDGET.subsec_nanos(),
            })),
        };
        let (outcome, cause) = match failure {
            RefreshError::Ambiguous(cause) => (RefreshOutcome::Ambiguous, cause),
            RefreshError::NotSubmitted(cause) => {
                self.set(Live::Ready(current));
                (RefreshOutcome::NotSubmitted, cause)
            }
            RefreshError::Rejected(cause) => {
                self.set(Live::Ready(current));
                (RefreshOutcome::Rejected, cause)
            }
        };
        Err(
            self.failure(CredentialCause::Refresh(CredentialCauseRefresh {
                outcome,
                cause,
            })),
        )
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
        let owner = HANDLES
            .lock_ignore_poison()
            .entry(key.clone())
            .or_insert_with(|| {
                Arc::new(Owned {
                    store: self.store.clone(),
                    id: key.1,
                    state: Mutex::new(State {
                        credential: Live::Ready(token.clone()).into(),
                        pending: None,
                    }),
                })
            })
            .clone();
        CredentialHandle(owner)
    }

    pub(super) fn credentials(&self) -> Result<Vec<CredentialHandle>, String> {
        let (_, tokens) = read_at(&self.store).map_err(|error| error.to_string())?;
        Ok(tokens
            .iter()
            .map(|token| {
                let handle = self.attach(token);
                handle.reconcile(token);
                handle
            })
            .collect())
    }

    fn publish(&self, token: &OAuthToken) -> CredentialHandle {
        let handle = self.attach(token);
        handle.set(Live::Ready(token.clone()));
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
        let (host, mut tokens) = read_at(&self.store).map_err(|error| error.to_string())?;
        let id = account_id(token);
        let replaced =
            if let Some(existing) = tokens.iter_mut().find(|entry| account_id(entry) == id) {
                *existing = token.clone();
                true
            } else {
                tokens.push(token.clone());
                false
            };
        write_all_at(&self.store, host.as_deref(), &tokens).map_err(|error| error.to_string())?;
        Ok((self.publish(token), replaced))
    }

    pub(super) fn host_id(&self) -> Result<String, String> {
        let (host, tokens) = read_at(&self.store).map_err(|error| error.to_string())?;
        if let Some(host) = host {
            return Ok(host);
        }
        let host = uuid::Uuid::new_v4().urn().to_string();
        write_all_at(&self.store, Some(&host), &tokens).map_err(|error| error.to_string())?;
        Ok(host)
    }

    pub(super) fn remove(&self, account: &str) -> Result<Option<OAuthToken>, String> {
        let (host, mut tokens) = read_at(&self.store).map_err(|error| error.to_string())?;
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
            self.store
                .remove(super::TOKEN_FILE)
                .map_err(|error| error.to_string())?;
        } else {
            write_all_at(&self.store, host.as_deref(), &tokens)
                .map_err(|error| error.to_string())?;
        }
        self.retire(&removed);
        Ok(Some(removed))
    }

    pub(super) fn clear(&self) -> Result<Vec<OAuthToken>, String> {
        let (_, tokens) = read_at(&self.store).map_err(|error| error.to_string())?;
        self.store
            .remove(super::TOKEN_FILE)
            .map_err(|error| error.to_string())?;
        for token in &tokens {
            self.retire(token);
        }
        Ok(tokens)
    }
}

async fn lease(store: &OwnDir, deadline: Instant) -> Result<StoreLease, CredentialCause> {
    timeout_at(deadline, async {
        let gate = GATE.lock().await;
        // Never replace or unlink this rendezvous file.
        let file = store
            .open_write("oauth.lock")
            .map_err(|error| store_cause(CredentialStoreOperation::OpenLease, (&error).into()))?;
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
                    return Err(store_cause(
                        CredentialStoreOperation::LockLease,
                        ral_core::io::failure(&error),
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| CredentialCause::LeaseTimedOut)?
}

/// Synchronous callers are startup or background jobs, never a UI frame.
///
/// # Panics
/// On an async worker, which the wait would block or deadlock.
pub(super) fn transaction<R: Send + 'static>(
    store: &OwnDir,
    change: impl FnOnce(&StoreLease) -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    let store = store.clone();
    let (send, receive) = oneshot::channel();
    EXECUTOR.spawn(async move {
        let result = async {
            let lease = lease(&store, Instant::now() + DEADLINE)
                .await
                .map_err(|cause| cause.describe())?;
            change(&lease)
        }
        .await;
        let _ = send.send(result);
    });
    receive
        .blocking_recv()
        .map_err(|_| "OAuth transaction owner stopped".to_string())?
}

fn store_cause(
    operation: CredentialStoreOperation,
    cause: ral_core::fact::IoCause,
) -> CredentialCause {
    CredentialCause::Store(CredentialCauseStore { operation, cause })
}
