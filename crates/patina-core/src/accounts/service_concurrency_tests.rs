use super::super::credentials::{CredentialRecord, CredentialStore};
use super::*;
use std::sync::{Condvar, Mutex, atomic::AtomicUsize};

struct BlockingBackend {
    records: Arc<MemoryCredentials>,
    writes: AtomicUsize,
    block_at: usize,
    reached: AtomicUsize,
    release: (Mutex<bool>, Condvar),
}
impl BlockingBackend {
    fn new(records: Arc<MemoryCredentials>, block_at: usize) -> Arc<Self> {
        Arc::new(Self {
            records,
            writes: AtomicUsize::new(0),
            block_at,
            reached: AtomicUsize::new(0),
            release: (Mutex::new(false), Condvar::new()),
        })
    }
    async fn wait(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while self.reached.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}
impl super::super::credentials::CredentialBackend for BlockingBackend {
    fn read(&self, r: &str) -> Result<Option<CredentialRecord>, CredentialError> {
        self.records.read(r)
    }
    fn delete(&self, r: &str) -> Result<(), CredentialError> {
        if self.block_at == 0 && self.records.read(r)?.is_some() {
            self.reached.store(1, Ordering::SeqCst);
            let guard = self.release.0.lock().unwrap();
            let (guard, timeout) = self
                .release
                .1
                .wait_timeout_while(guard, std::time::Duration::from_secs(5), |released| {
                    !*released
                })
                .unwrap();
            assert!(
                !timeout.timed_out() && *guard,
                "cleanup fixture not released"
            );
        }
        self.records.delete(r)
    }
    fn write(&self, r: &str, record: &CredentialRecord) -> Result<(), CredentialError> {
        if self.writes.fetch_add(1, Ordering::SeqCst) + 1 == self.block_at {
            self.reached.store(1, Ordering::SeqCst);
            let guard = self.release.0.lock().unwrap();
            let (guard, timeout) = self
                .release
                .1
                .wait_timeout_while(guard, std::time::Duration::from_secs(5), |released| {
                    !*released
                })
                .unwrap();
            assert!(
                !timeout.timed_out() && *guard,
                "blocked fixture not released"
            );
        }
        self.records.write(r, record)
    }
}
fn replace_backend(f: &mut Fixture, backend: Arc<BlockingBackend>) {
    let stages = [
        AuthStage::DeviceCode,
        AuthStage::MicrosoftToken,
        AuthStage::XboxLive,
        AuthStage::Xsts,
        AuthStage::MinecraftToken,
        AuthStage::Entitlement,
        AuthStage::Profile,
    ];
    let http = AuthHttp::for_test(std::array::from_fn(|i| {
        (stages[i], format!("{}/{i}", f.server.uri()))
    }))
    .unwrap();
    f.service = AccountService::for_test(
        f.data.clone(),
        MsaClient::for_test(http, f.clock.clone()),
        CredentialStore::new(
            f.data.clone(),
            backend,
            Arc::new(MemoryCredentials::default()),
        ),
        f.clock.clone(),
    );
}

#[tokio::test]
async fn dropping_refresh_waiter_cannot_drop_rotation_save_or_start_downstream() {
    let mut f = Fixture::new().await;
    let backend = BlockingBackend::new(f.keyring.clone(), 2);
    replace_backend(&mut f, backend.clone());
    let account = f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    let service = f.service.clone();
    let (_tx, cancel) = watch::channel(false);
    let waiter = tokio::spawn(async move { service.resolve_launch(ID, cancel).await });
    backend.wait().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    backend.release();
    f.service.reload().await.unwrap(); // waits for the owned critical task
    assert_eq!(
        f.keyring
            .read(account.token_ref.as_ref().unwrap())
            .unwrap()
            .unwrap()
            .refresh_token()
            .expose(),
        "ROTATED_SENTINEL"
    );
    let requests = f.server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "cancelled waiter must not continue downstream after saving rotation"
    );
}

#[tokio::test]
async fn cancellation_before_login_commit_cleans_staged_secret_and_has_one_terminal() {
    let mut f = Fixture::new().await;
    let backend = BlockingBackend::new(f.keyring.clone(), 1);
    replace_backend(&mut f, backend.clone());
    f.chain("Renamed").await;
    let (events, mut received) = mpsc::unbounded_channel();
    let (tx, cancel) = watch::channel(false);
    let waiter = tokio::spawn(f.service.start(events, cancel));
    backend.wait().await;
    tx.send(true).unwrap();
    backend.release();
    waiter.await.unwrap();
    let mut terminal = vec![];
    while let Some(e) = received.recv().await {
        if matches!(e, LoginEvent::Done { .. } | LoginEvent::Failed(_)) {
            terminal.push(e);
        }
    }
    assert_eq!(
        terminal,
        vec![LoginEvent::Failed(
            super::super::login::LoginError::Cancelled
        )]
    );
    assert!(
        f.restarted()
            .snapshot()
            .await
            .unwrap()
            .accounts()
            .is_empty()
    );
    assert_eq!(
        crate::platform::credentials::PrivateCredentialDir::open(&f.data)
            .unwrap()
            .read("cleanup.json")
            .unwrap()
            .unwrap(),
        b"[]"
    );
}

#[tokio::test]
async fn view_closed_while_queued_commit_cancels_without_waiting_for_lock() {
    let f = Fixture::new().await;
    f.chain("Renamed").await;
    let guard = crate::platform::credentials::PrivateCredentialDir::open(&f.data)
        .unwrap()
        .try_lock()
        .unwrap()
        .unwrap();
    let (events, received) = mpsc::unbounded_channel();
    let (_tx, cancel) = watch::channel(false);
    let waiter = tokio::spawn(f.service.start(events, cancel));
    while f.server.received_requests().await.unwrap().len() < 7 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    drop(received);
    let result = tokio::time::timeout(std::time::Duration::from_millis(500), waiter).await;
    drop(guard);
    assert!(
        result.is_ok(),
        "closed view kept waiting for transaction lock"
    );
    assert!(
        f.restarted()
            .snapshot()
            .await
            .unwrap()
            .accounts()
            .is_empty()
    );
}

#[tokio::test]
async fn fresh_session_metadata_failure_retries_metadata_not_stale_cached_name() {
    let f = Fixture::new().await;
    f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    f.service
        .set_metadata_fault(crate::platform::credentials::WriteStage::Rename);
    assert_eq!(f.resolve().await.unwrap_err(), AuthError::CredentialWrite);
    let grant = f.resolve().await.unwrap();
    assert_eq!(grant.account().name(), "Renamed");
    assert_eq!(
        f.restarted()
            .snapshot()
            .await
            .unwrap()
            .current()
            .unwrap()
            .username,
        "Renamed"
    );
    assert_eq!(f.refresh_count().await, 1);
}

#[tokio::test]
async fn exact_not_after_and_refresh_complete_expiry_do_not_request_downstream() {
    use super::super::secret::SecretString;
    let f = Fixture::new().await;
    f.chain("Renamed").await;
    let stages = [
        AuthStage::DeviceCode,
        AuthStage::MicrosoftToken,
        AuthStage::XboxLive,
        AuthStage::Xsts,
        AuthStage::MinecraftToken,
        AuthStage::Entitlement,
        AuthStage::Profile,
    ];
    let client = MsaClient::for_test(
        AuthHttp::for_test(std::array::from_fn(|i| {
            (stages[i], format!("{}/{i}", f.server.uri()))
        }))
        .unwrap(),
        f.clock.clone(),
    );
    let (_tx, cancel) = watch::channel(false);
    let tokens = client
        .refresh_microsoft(
            SecretString::new("OLD_SECRET_SENTINEL".into()),
            cancel.clone(),
        )
        .await
        .unwrap();
    f.clock.0.store(NOW + 3600, Ordering::SeqCst);
    assert_eq!(
        client.complete_minecraft(tokens, cancel).await.unwrap_err(),
        AuthError::SessionExpired
    );
    assert_eq!(f.server.received_requests().await.unwrap().len(), 1);
    f.clock.0.store(1_893_456_000, Ordering::SeqCst); // exactly 2030-01-01 NotAfter
    let (_tx, cancel) = watch::channel(false);
    let tokens = client
        .refresh_microsoft(
            SecretString::new("OLD_SECRET_SENTINEL".into()),
            cancel.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        client.complete_minecraft(tokens, cancel).await.unwrap_err(),
        AuthError::InvalidResponse {
            stage: AuthStage::XboxLive
        }
    );
    assert_eq!(f.server.received_requests().await.unwrap().len(), 3); // no XSTS
}

#[tokio::test]
async fn short_minecraft_expiry_never_requests_profile_with_expired_bearer() {
    let f = Fixture::new().await;
    f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    Mock::given(path("/4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token":"MC_SECRET_SENTINEL","expires_in":1,"token_type":"Bearer"}),
        ))
        .with_priority(1)
        .mount(&f.server)
        .await;
    let clock = f.clock.clone();
    Mock::given(path("/5"))
        .respond_with(move |_: &wiremock::Request| {
            clock.0.store(NOW + 1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({"items":[{"name":"game_minecraft"}]}))
        })
        .with_priority(1)
        .mount(&f.server)
        .await;
    assert_eq!(f.resolve().await.unwrap_err(), AuthError::SessionExpired);
    assert!(
        !f.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path() == "/6")
    );
}

#[tokio::test]
async fn closing_view_after_commit_keeps_publication_and_durable_current_account() {
    let mut f = Fixture::new().await;
    let backend = BlockingBackend::new(f.keyring.clone(), 0);
    replace_backend(&mut f, backend.clone());
    let old = f.seed(NOW + 3600).await;
    f.chain("Renamed").await;
    let before = *f.service.changes().borrow();
    let (events, received) = mpsc::unbounded_channel();
    let (tx, cancel) = watch::channel(false);
    let waiter = tokio::spawn(f.service.start(events, cancel));
    backend.wait().await; // old-reference cleanup follows durable commit/publication
    let snapshot = f.service.snapshot().await.unwrap();
    assert_eq!(snapshot.current().unwrap().username, "Renamed");
    assert_eq!(snapshot.revision(), before + 1);
    assert_ne!(snapshot.current().unwrap().token_ref, old.token_ref);
    tx.send(true).unwrap();
    drop(received);
    backend.release();
    waiter.await.unwrap();
    let restarted = f.restarted().snapshot().await.unwrap();
    assert_eq!(restarted.current().unwrap().username, "Renamed");
    assert_eq!(*f.service.changes().borrow(), before + 1);
}

#[tokio::test]
async fn long_device_polling_does_not_hold_account_mutation_lock() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    f.chain("Renamed").await;
    let (events, mut received) = mpsc::unbounded_channel();
    let (tx, cancel) = watch::channel(false);
    let attempt = tokio::spawn(f.service.start(events, cancel));
    assert!(matches!(received.recv().await, Some(LoginEvent::Code(_))));
    assert_eq!(received.recv().await, Some(LoginEvent::Waiting));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_millis(500), f.service.remove(ID))
            .await
            .unwrap()
            .unwrap(),
        super::super::RemovalOutcome::Removed
    );
    tx.send(true).unwrap();
    attempt.await.unwrap();
    assert_eq!(
        received.recv().await,
        Some(LoginEvent::Failed(
            super::super::login::LoginError::Cancelled
        ))
    );
    assert_eq!(received.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn cross_process_lock_retries_are_bounded_and_cancellable() {
    // Keep paused time explicit while spawn_blocking performs actual lock IO.
    struct Running(tokio::task::JoinHandle<()>);
    impl Drop for Running {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _running = Running(tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    }));
    let f = Fixture::new().await;
    let lock = crate::platform::credentials::PrivateCredentialDir::open(&f.data)
        .unwrap()
        .try_lock()
        .unwrap()
        .unwrap();
    let service = f.service.clone();
    let (_tx, cancel) = watch::channel(false);
    let waiter = tokio::spawn(async move { service.resolve_launch(ID, cancel).await });
    // Allow the operation task to initialize its monotonic deadline and reach
    // the blocking-pool call before advancing. No auth requests are possible.
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    for _ in 0..299 {
        tokio::time::advance(std::time::Duration::from_millis(100)).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }
    assert!(
        !waiter.is_finished(),
        "lock deadline expired before 30 seconds"
    );
    tokio::time::advance(std::time::Duration::from_millis(100)).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !waiter.is_finished() {
        assert!(std::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert_eq!(waiter.await.unwrap().unwrap_err(), AuthError::Busy);
    assert!(f.server.received_requests().await.unwrap().is_empty());
    drop(lock);
}

#[tokio::test]
async fn dropping_queued_removal_cancels_before_any_mutation() {
    let f = Fixture::new().await;
    f.seed(NOW + 3600).await;
    let lock = crate::platform::credentials::PrivateCredentialDir::open(&f.data)
        .unwrap()
        .try_lock()
        .unwrap()
        .unwrap();
    let service = f.service.clone();
    let waiter = tokio::spawn(async move { service.remove(ID).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    drop(lock);
    f.service.reload().await.unwrap();
    assert_eq!(
        f.service.snapshot().await.unwrap().current().unwrap().id,
        ID
    );
}

#[tokio::test]
async fn rotation_save_finishing_at_microsoft_expiry_prevents_xbox_request() {
    let mut f = Fixture::new().await;
    let backend = BlockingBackend::new(f.keyring.clone(), 2);
    replace_backend(&mut f, backend.clone());
    let account = f.seed(NOW + 60).await;
    f.chain("Renamed").await;
    let service = f.service.clone();
    let (_tx, cancel) = watch::channel(false);
    let waiter = tokio::spawn(async move { service.resolve_launch(ID, cancel).await });
    backend.wait().await;
    f.clock.0.store(NOW + 3600, Ordering::SeqCst);
    backend.release();
    assert_eq!(
        waiter.await.unwrap().unwrap_err(),
        AuthError::SessionExpired
    );
    assert_eq!(f.server.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        f.keyring
            .read(account.token_ref.as_ref().unwrap())
            .unwrap()
            .unwrap()
            .refresh_token()
            .expose(),
        "ROTATED_SENTINEL"
    );
}
