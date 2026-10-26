use super::*;
use crate::provider::oauth::{self, Grant, TOKEN_FILE, grok};
use ral_core::own::TestDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

struct Request {
    body: String,
    reply: oneshot::Sender<&'static str>,
}

struct Issuer {
    url: String,
    requests: mpsc::UnboundedReceiver<Request>,
    task: tokio::task::JoinHandle<()>,
}

impl Issuer {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let (send, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let body = loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&buf[..n]);
                    if let Some(head) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers =
                            String::from_utf8_lossy(&request[..head]).to_ascii_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if request.len() >= head + 4 + length {
                            break String::from_utf8(request[head + 4..].to_vec()).unwrap();
                        }
                    }
                };
                let (reply, receive) = oneshot::channel();
                if send.send(Request { body, reply }).is_err() {
                    return;
                }
                let Ok(body) = receive.await else { return };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    async fn request(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

impl Drop for Issuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const FRESH: &str = r#"{"access_token":"a1","refresh_token":"r1","expires_in":3600}"#;
const NEXT: &str = r#"{"access_token":"a2","refresh_token":"r2","expires_in":3600}"#;

fn token() -> OAuthToken {
    OAuthToken {
        access_token: "a0".into(),
        refresh_token: "r0".into(),
        issued: "account".into(),
        email: None,
        expires_at: 0,
        grant: Grant::Grok,
    }
}

#[test]
fn reconciliation_preserves_unresolved_outcomes_until_disk_changes() {
    let predecessor = token();
    let successor = OAuthToken {
        access_token: "a1".into(),
        refresh_token: "r1".into(),
        ..predecessor.clone()
    };
    let replacement = OAuthToken {
        access_token: "another-login".into(),
        refresh_token: "another-refresh".into(),
        ..predecessor.clone()
    };
    for state in [
        CredentialState::Ready(predecessor.clone()),
        CredentialState::Uncertain(predecessor.clone()),
        CredentialState::Unsaved {
            predecessor: predecessor.clone(),
            successor: successor.clone(),
        },
    ] {
        assert_eq!(state.reconcile(&predecessor), state);
        assert_eq!(
            state.reconcile(&replacement),
            CredentialState::Ready(replacement.clone())
        );
    }
    let unsaved = CredentialState::Unsaved {
        predecessor: predecessor.clone(),
        successor: successor.clone(),
    };
    assert_eq!(unsaved.token(), &successor);
    assert_eq!(
        unsaved.reconcile(&successor),
        CredentialState::Ready(successor)
    );
    let retired = CredentialState::Retired(predecessor.clone());
    assert_eq!(
        retired.reconcile(&replacement),
        CredentialState::Ready(replacement)
    );
    assert_eq!(
        retired.reconcile(&predecessor),
        CredentialState::Ready(predecessor)
    );
}

fn fixture() -> (TestDir, CredentialHandle) {
    let state = OwnDir::for_test("oauth-owner").unwrap();
    let handle = oauth::commit_for_test(&state, &token());
    (state, handle)
}

async fn prepare(handle: &CredentialHandle, url: String) -> Result<(), String> {
    handle
        .prepare_until(
            Instant::now() + Duration::from_secs(5),
            async move |current| {
                let client = reqwest::Client::builder().no_proxy().build().unwrap();
                grok::refresh_at(&client, &url, &current)
                    .await
                    .map(|update| update.finish(&current, |()| Grant::Grok))
            },
        )
        .await
}

#[tokio::test]
async fn concurrent_waiters_share_rotation_and_the_next_exchange_uses_its_successor() {
    let (state, handle) = fixture();
    let mut issuer = Issuer::new().await;
    let first = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    let request = issuer.request().await;
    assert!(request.body.contains("refresh_token=r0"));
    let second = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    request.reply.send(FRESH).unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert!(issuer.requests.try_recv().is_err());
    assert_eq!(handle.snapshot().access_token, "a1");
    let mut saved = read_at(&state).unwrap().1.remove(0);
    assert_eq!(saved.refresh_token, "r1");
    saved.expires_at = 0;
    oauth::commit_for_test(&state, &saved);
    let next = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    let request = issuer.request().await;
    assert!(request.body.contains("refresh_token=r1"));
    request.reply.send(NEXT).unwrap();
    next.await.unwrap().unwrap();
    assert_eq!(read_at(&state).unwrap().1[0].refresh_token, "r2");
}

#[tokio::test]
async fn cancelling_every_waiter_does_not_discard_a_submitted_rotation() {
    let (state, handle) = fixture();
    let mut issuer = Issuer::new().await;
    let waiter = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    let request = issuer.request().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let survivor = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    request.reply.send(FRESH).unwrap();
    survivor.await.unwrap().unwrap();
    assert_eq!(handle.snapshot().refresh_token, "r1");
    assert_eq!(read_at(&state).unwrap().1[0].refresh_token, "r1");
    assert!(issuer.requests.try_recv().is_err());
}

#[tokio::test]
async fn queued_login_wins_and_delayed_admission_cannot_restore_the_old_snapshot() {
    let (state, handle) = fixture();
    let mut issuer = Issuer::new().await;
    let refreshing = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    let request = issuer.request().await;
    let mut login = token();
    login.access_token = "new-login".into();
    login.refresh_token = "new-refresh".into();
    login.expires_at = u64::MAX;
    let state_clone = (*state).clone();
    let committed =
        tokio::task::spawn_blocking(move || oauth::commit_for_test(&state_clone, &login));
    request.reply.send(FRESH).unwrap();
    refreshing.await.unwrap().unwrap();
    let renewed = committed.await.unwrap();
    assert!(handle.shares_owner(&renewed));
    assert_eq!(handle.snapshot().access_token, "new-login");
    let mut credentials = crate::provider::credential::CredentialStore::default();
    let account = credentials.add_login(&handle).unwrap();
    let Some(crate::provider::credential::Credential::OAuth(current)) =
        credentials.get(&account.id)
    else {
        panic!("missing login")
    };
    assert_eq!(current.snapshot().refresh_token, "new-refresh");
}

#[tokio::test]
async fn queued_logout_wins_and_cached_owners_cannot_resurrect_it() {
    let (state, handle) = fixture();
    let mut issuer = Issuer::new().await;
    let refreshing = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    let request = issuer.request().await;
    let store = (*state).clone();
    let removed = tokio::task::spawn_blocking(move || oauth::remove_at(&store, "account"));
    request.reply.send(FRESH).unwrap();
    refreshing.await.unwrap().unwrap();
    assert_eq!(removed.await.unwrap().unwrap().unwrap().refresh_token, "r1");
    assert!(handle.is_retired());
    assert!(prepare(&handle, issuer.url.clone()).await.is_err());
    assert_eq!(read_at(&state).unwrap().1, []);
    assert!(issuer.requests.try_recv().is_err());
    assert!(
        crate::provider::credential::CredentialStore::default()
            .add_login(&handle)
            .is_err()
    );
}

#[tokio::test]
async fn failed_persistence_keeps_the_successor_and_retries_only_its_write() {
    let (state, handle) = fixture();
    let mut issuer = Issuer::new().await;
    let refreshing = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    let request = issuer.request().await;
    state.remove(TOKEN_FILE).unwrap();
    state.make_dir(TOKEN_FILE).unwrap();
    request.reply.send(FRESH).unwrap();
    assert!(refreshing.await.unwrap().is_err());
    assert_eq!(handle.snapshot().refresh_token, "r1");
    std::fs::remove_dir(state.path().join(TOKEN_FILE)).unwrap();
    write_all_at(&state, None, &[token()]).unwrap();
    prepare(&handle, issuer.url.clone()).await.unwrap();
    assert_eq!(read_at(&state).unwrap().1[0].refresh_token, "r1");
    assert!(issuer.requests.try_recv().is_err());
}

#[tokio::test]
async fn an_ambiguous_exchange_is_not_replayed() {
    let (_state, handle) = fixture();
    let mut issuer = Issuer::new().await;
    let url = issuer.url.clone();
    let refreshing = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .prepare_until(
                    Instant::now() + Duration::from_secs(2),
                    async move |current| {
                        grok::refresh_at(&reqwest::Client::new(), &url, &current)
                            .await
                            .map(|update| update.finish(&current, |()| Grant::Grok))
                    },
                )
                .await
        }
    });
    let _request = issuer.request().await;
    assert!(refreshing.await.unwrap().is_err());
    assert!(prepare(&handle, issuer.url.clone()).await.is_err());
    assert!(issuer.requests.try_recv().is_err());
}

#[tokio::test]
async fn a_held_file_lock_obeys_the_operation_deadline_without_submitting_http() {
    let (state, handle) = fixture();
    let file = state.open_write("oauth.lock").unwrap();
    file.try_lock().unwrap();
    let result = handle
        .prepare_until(
            Instant::now() + Duration::from_millis(100),
            async |_| -> Result<OAuthToken, String> { panic!("issuer called while locked") },
        )
        .await;
    assert!(result.is_err());
    drop(file);
    let mut issuer = Issuer::new().await;
    let retry = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    issuer.request().await.reply.send(FRESH).unwrap();
    retry.await.unwrap().unwrap();
}

#[tokio::test]
async fn loading_waits_for_removal_before_attaching_any_owner() {
    let store = OwnDir::for_test("oauth-attachment").unwrap();
    let lease = lease(&store, Instant::now() + DEADLINE).await.unwrap();
    write_all_at(&store, None, &[token()]).unwrap();
    let (started, waiting) = oneshot::channel();
    let loading = tokio::task::spawn_blocking({
        let store = (*store).clone();
        move || {
            started.send(()).unwrap();
            transaction(&store, StoreLease::credentials)
        }
    });
    waiting.await.unwrap();
    assert!(!loading.is_finished());
    assert!(lease.remove("account").unwrap().is_some());
    drop(lease);
    assert!(loading.await.unwrap().unwrap().is_empty());
    assert!(
        !HANDLES
            .lock_ignore_poison()
            .contains_key(&(store.path().to_path_buf(), account_id(&token())))
    );
}

#[test]
fn process_login() {
    if std::env::var_os("RAL_OAUTH_TEST_LOGIN").is_none() {
        return;
    }
    let store = oauth::state().unwrap();
    oauth::commit_for_test(&store, &external_login());
}

fn external_login() -> OAuthToken {
    OAuthToken {
        access_token: "new-login".into(),
        refresh_token: "new-refresh".into(),
        expires_at: u64::MAX,
        ..token()
    }
}

#[tokio::test]
async fn another_process_can_renew_a_retired_owner_by_preparation_or_loading() {
    for reload in [false, true] {
        let root = OwnDir::for_test("oauth-external-login").unwrap();
        let store = root.make_dir("exarch").unwrap();
        let owner = oauth::commit_for_test(&store, &token());
        let removal = lease(&store, Instant::now() + DEADLINE).await.unwrap();
        removal.remove("account").unwrap().unwrap();
        drop(removal);
        assert!(owner.is_retired());
        assert!(
            owner
                .prepare_until(
                    Instant::now() + DEADLINE,
                    async |_| -> Result<OAuthToken, String> { panic!("removed account refreshed") },
                )
                .await
                .is_err()
        );
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "provider::oauth::session::tests::process_login"])
                .env("XDG_STATE_HOME", root.path())
                .env("RAL_OAUTH_TEST_LOGIN", "1")
                .spawn()
                .unwrap(),
        );
        child.finish().await;
        assert!(
            owner.is_retired(),
            "a peer does not mutate this process's memory"
        );
        if reload {
            let lease = lease(&store, Instant::now() + DEADLINE).await.unwrap();
            let loaded = lease.credentials().unwrap().remove(0);
            assert!(owner.shares_owner(&loaded));
            drop(lease);
        } else {
            owner
                .prepare_until(
                    Instant::now() + DEADLINE,
                    async |_| -> Result<OAuthToken, String> { panic!("fresh login refreshed") },
                )
                .await
                .unwrap();
        }
        assert!(!owner.is_retired());
        assert_eq!(owner.snapshot(), external_login());
    }
}

#[test]
fn process_lock() {
    if std::env::var_os("RAL_OAUTH_TEST_LOCK").is_none() {
        return;
    }
    let store = oauth::state().unwrap();
    let file = store.open_write("oauth.lock").unwrap();
    file.try_lock().unwrap();
    drop(store.open_write("held").unwrap());
    loop {
        std::thread::park();
    }
}

#[tokio::test]
async fn killing_a_lock_holder_releases_the_lease_without_deleting_its_file() {
    let root = OwnDir::for_test("oauth-lock-holder").unwrap();
    let store = root.make_dir("exarch").unwrap();
    let handle = oauth::commit_for_test(&store, &token());
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "provider::oauth::session::tests::process_lock"])
            .env("XDG_STATE_HOME", root.path())
            .env("RAL_OAUTH_TEST_LOCK", "1")
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::metadata(store.path().join("held")).is_err() {
        assert!(
            Instant::now() < deadline,
            "lock holder did not become ready"
        );
        sleep(Duration::from_millis(10)).await;
    }
    assert!(
        handle
            .prepare_until(
                Instant::now() + Duration::from_millis(100),
                async |_| -> Result<OAuthToken, String> { panic!("locked issuer") }
            )
            .await
            .is_err()
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let mut issuer = Issuer::new().await;
    let refreshing = tokio::spawn({
        let handle = handle.clone();
        let url = issuer.url.clone();
        async move { prepare(&handle, url).await }
    });
    issuer.request().await.reply.send(FRESH).unwrap();
    refreshing.await.unwrap().unwrap();
}

#[test]
fn process_refresh() {
    let Some(url) = std::env::var_os("RAL_OAUTH_TEST_ISSUER") else {
        return;
    };
    let store = oauth::state().unwrap();
    let owner = transaction(&store, StoreLease::credentials)
        .unwrap()
        .remove(0);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(prepare(&owner, url.into_string().unwrap()))
        .unwrap();
    assert_eq!(owner.snapshot().refresh_token, "r1");
}

struct Child(std::process::Child);

impl Child {
    async fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "OAuth child did not finish");
            sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn two_processes_share_one_rotation_and_preserve_sibling_accounts_and_host() {
    let root = OwnDir::for_test("oauth-processes").unwrap();
    let store = root.make_dir("exarch").unwrap();
    let mut sibling = token();
    sibling.issued = "sibling".into();
    sibling.expires_at = u64::MAX;
    write_all_at(&store, Some("host-id"), &[token(), sibling.clone()]).unwrap();
    let mut issuer = Issuer::new().await;
    let start = || {
        Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "provider::oauth::session::tests::process_refresh",
                    "--nocapture",
                ])
                .env("XDG_STATE_HOME", root.path())
                .env("RAL_OAUTH_TEST_ISSUER", &issuer.url)
                .spawn()
                .unwrap(),
        )
    };
    let mut first = start();
    let mut second = start();
    let request = issuer.request().await;
    assert!(request.body.contains("refresh_token=r0"));
    request.reply.send(FRESH).unwrap();
    first.finish().await;
    second.finish().await;
    assert!(issuer.requests.try_recv().is_err());
    let (host, saved) = read_at(&store).unwrap();
    assert_eq!(host.as_deref(), Some("host-id"));
    assert_eq!(saved.len(), 2);
    assert_eq!(
        saved
            .iter()
            .find(|token| token.issued == "account")
            .unwrap()
            .refresh_token,
        "r1"
    );
    assert_eq!(
        saved
            .iter()
            .find(|token| token.issued == "sibling")
            .unwrap(),
        &sibling
    );
}
