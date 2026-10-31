//! Single mutation owner. Critical operations run in owned Tokio tasks: dropping a
//! waiter cannot interrupt an acknowledged refresh rotation or a metadata commit.
use super::{
    Account, AccountKind, AccountStore, OfflineOrigin, Verification,
    credentials::{CredentialRecord, CredentialStore},
    error::AuthError,
    http::cancelled,
    login::{CancelToken, DeviceCodeLogin, LoginError, LoginEvent, StorageKind},
    msa::{AuthClock, MinecraftSession, MsaClient, SystemClock, VerifiedLogin},
    policy::{is_launchable, offline_allowed},
};
use crate::{
    launch::{
        LaunchAccount,
        session::{GameSession, PendingLaunch, SessionError},
    },
    paths::DataDirs,
    platform::credentials::CredentialLock,
    progress::ProgressSink,
};
use futures::future::BoxFuture;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc::UnboundedSender, watch};
use uuid::Uuid;

mod skin;
mod transactions;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSnapshot {
    accounts: Vec<Account>,
    current: Option<String>,
    revision: u64,
}
impl AccountSnapshot {
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }
    pub fn current(&self) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| Some(&a.id) == self.current.as_ref())
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn is_offline_allowed(&self) -> bool {
        offline_allowed(&self.accounts)
    }
    pub fn is_launchable(&self, id: &str) -> bool {
        self.accounts
            .iter()
            .any(|a| a.id == id && is_launchable(a, &self.accounts))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalOutcome {
    Removed,
    CleanupPending,
}

/// Opaque, nonserializable authorization bound to the exact credential session.
#[derive(Debug)]
pub struct LaunchGrant {
    account: LaunchAccount,
    id: String,
    reference: Option<String>,
    offline_origin: Option<OfflineOrigin>,
    issued_at: u64,
    expires_at: u64,
}
impl LaunchGrant {
    pub fn account(&self) -> &LaunchAccount {
        &self.account
    }
}

#[derive(thiserror::Error)]
pub enum AuthenticatedLaunchError {
    #[error("{0}")]
    Auth(AuthError),
    // Keep the typed legacy error for recovery, never its arbitrary payload in diagnostics.
    #[error("{}", rust_i18n::t!("errors.auth.launch"))]
    Session(SessionError),
}
impl std::fmt::Debug for AuthenticatedLaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AuthenticatedLaunchError({self})")
    }
}

#[derive(Default)]
struct OperationState {
    pending: HashMap<String, CredentialRecord>,
    revoked: HashSet<String>,
    metadata: HashMap<String, Account>,
    retained_lock: Option<CredentialLock>,
}
struct Operation {
    state: OwnedMutexGuard<OperationState>,
    lock: Option<CredentialLock>,
}
impl Drop for Operation {
    fn drop(&mut self) {
        // Do not let another process exchange a stale token while the newest
        // rotation exists only here. Retry storage or remove/relogin explicitly.
        if !self.state.pending.is_empty()
            || !self.state.revoked.is_empty()
            || !self.state.metadata.is_empty()
        {
            self.state.retained_lock = self.lock.take();
        }
    }
}
struct Inner {
    data: DataDirs,
    client: MsaClient,
    skin: super::skin::SkinClient,
    skin_decodes: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    skin_decode_probe: std::sync::Mutex<Option<Arc<skin::DecodeProbe>>>,
    credentials: Arc<CredentialStore>,
    clock: Arc<dyn AuthClock>,
    operation: Arc<Mutex<OperationState>>,
    snapshot: RwLock<Option<Result<AccountSnapshot, AuthError>>>,
    changes: watch::Sender<u64>,
    #[cfg(test)]
    pub(super) metadata_fault: std::sync::Mutex<Option<crate::platform::credentials::WriteStage>>,
}
#[derive(Clone)]
pub struct AccountService {
    inner: Arc<Inner>,
}

struct CancelOnDrop(watch::Sender<bool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}
struct CancelRelay(tokio::task::JoinHandle<()>);
impl Drop for CancelRelay {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn relay_cancel(
    mut external: CancelToken,
    events: Option<UnboundedSender<LoginEvent>>,
) -> (CancelOnDrop, CancelToken, CancelRelay) {
    let (tx, rx) = watch::channel(check_cancel(&external).is_err());
    let signal = tx.clone();
    let relay = tokio::spawn(async move {
        if let Some(events) = events {
            tokio::select! { _ = cancelled(&mut external) => {}, _ = events.closed() => {} }
        } else {
            cancelled(&mut external).await;
        }
        let _ = signal.send(true);
    });
    (CancelOnDrop(tx), rx, CancelRelay(relay))
}
fn check_cancel(cancel: &CancelToken) -> Result<(), AuthError> {
    if *cancel.borrow() || cancel.has_changed().is_err() {
        Err(AuthError::Cancelled)
    } else {
        Ok(())
    }
}
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
}
impl AccountService {
    /// Lazy: no filesystem or keyring probing, and no authentication request.
    pub fn production(data: DataDirs) -> Result<Self, AuthError> {
        Ok(Self::new(
            data.clone(),
            MsaClient::production()?,
            CredentialStore::production(data),
            Arc::new(SystemClock),
        ))
    }
    fn new(
        data: DataDirs,
        client: MsaClient,
        credentials: CredentialStore,
        clock: Arc<dyn AuthClock>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                data,
                client,
                skin: super::skin::SkinClient::production(),
                skin_decodes: Arc::new(tokio::sync::Semaphore::new(1)),
                #[cfg(test)]
                skin_decode_probe: std::sync::Mutex::new(None),
                credentials: Arc::new(credentials),
                clock,
                operation: Arc::new(Mutex::new(OperationState::default())),
                snapshot: RwLock::new(None),
                changes: watch::channel(0).0,
                #[cfg(test)]
                metadata_fault: std::sync::Mutex::new(None),
            }),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        data: DataDirs,
        client: MsaClient,
        credentials: CredentialStore,
        clock: Arc<dyn AuthClock>,
    ) -> Self {
        Self::new(data, client, credentials, clock)
    }
    #[cfg(test)]
    pub(super) fn set_metadata_fault(&self, stage: crate::platform::credentials::WriteStage) {
        *self.inner.metadata_fault.lock().expect("metadata fault") = Some(stage);
    }
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }
    pub async fn snapshot(&self) -> Result<AccountSnapshot, AuthError> {
        if let Some(snapshot) = self
            .inner
            .snapshot
            .read()
            .expect("account snapshot")
            .clone()
        {
            return snapshot;
        }
        self.reload().await?;
        self.inner
            .snapshot
            .read()
            .expect("account snapshot")
            .clone()
            .expect("loaded snapshot")
    }
    fn publish(&self, store: &AccountStore, force: bool) {
        let mut published = self.inner.snapshot.write().expect("account snapshot");
        let revision = *self.inner.changes.borrow();
        let mut next = AccountSnapshot {
            accounts: store.accounts.clone(),
            current: store.current.clone(),
            revision,
        };
        let changed = force
            || published
                .as_ref()
                .is_some_and(|old| old.as_ref() != Ok(&next));
        if changed {
            next.revision = revision.checked_add(1).expect("account revision overflow");
        }
        let revision = next.revision;
        *published = Some(Ok(next));
        if changed {
            self.inner.changes.send_replace(revision);
        }
    }

    pub async fn reload(&self) -> Result<(), AuthError> {
        let service = self.clone();
        let (tx, cancel) = watch::channel(false);
        let _cancel_on_drop = CancelOnDrop(tx);
        tokio::spawn(async move {
            let mut operation = service.lock(cancel.clone()).await?;
            let store = service.load().await?;
            let store = service.recover(&mut operation, store).await?;
            service.publish(&store, false);
            Ok(())
        })
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
    }
    pub async fn set_current(&self, id: &str) -> Result<(), AuthError> {
        let service = self.clone();
        let id = id.to_owned();
        let (tx, cancel) = watch::channel(false);
        let _cancel_on_drop = CancelOnDrop(tx);
        tokio::spawn(async move {
            let mut operation = service.lock(cancel.clone()).await?;
            let store = service.load().await?;
            let mut store = service.recover(&mut operation, store).await?;
            check_cancel(&cancel)?;
            let account = find(&store, &id)?;
            if !is_launchable(account, &store.accounts) {
                return Err(if account.kind == AccountKind::Offline {
                    AuthError::OfflineNotAllowed
                } else {
                    AuthError::ReauthenticationRequired
                });
            }
            store.set_current(&id).map_err(AuthError::Account)?;
            let store = service.save(&mut operation, store).await?;
            service.publish(&store, true);
            Ok(())
        })
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
    }
    pub async fn add_offline(&self, name: &str) -> Result<Account, AuthError> {
        self.add_offline_with_origin(name, OfflineOrigin::Entitled)
            .await
    }
    #[cfg(feature = "dev-offline")]
    pub async fn add_dev_offline(&self, name: &str) -> Result<Account, AuthError> {
        self.add_offline_with_origin(name, OfflineOrigin::Development)
            .await
    }
    async fn add_offline_with_origin(
        &self,
        name: &str,
        origin: OfflineOrigin,
    ) -> Result<Account, AuthError> {
        let service = self.clone();
        let name = name.to_owned();
        let (tx, cancel) = watch::channel(false);
        let _cancel_on_drop = CancelOnDrop(tx);
        tokio::spawn(async move {
            let mut operation = service.lock(cancel.clone()).await?;
            let store = service.load().await?;
            let mut store = service.recover(&mut operation, store).await?;
            check_cancel(&cancel)?;
            let account = store.insert_offline(&name, origin)?;
            let store = service.save(&mut operation, store).await?;
            service.publish(&store, true);
            Ok(account)
        })
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
    }
    pub async fn remove(&self, id: &str) -> Result<RemovalOutcome, AuthError> {
        let service = self.clone();
        let id = id.to_owned();
        let (tx, cancel) = watch::channel(false);
        let _cancel_on_drop = CancelOnDrop(tx);
        tokio::spawn(async move {
            let mut operation = service.lock(cancel.clone()).await?;
            // Removal may discard an unsaved rotation, but only after durable
            // removal of its reference. It must not require storage to recover first.
            let mut store = service.load().await?;
            check_cancel(&cancel)?;
            let account = find(&store, &id)?.clone();
            service
                .journal(account.token_ref.clone().into_iter().collect())
                .await?;
            check_cancel(&cancel)?;
            store.remove(&id).map_err(AuthError::Account)?;
            let store = service.save(&mut operation, store).await?;
            if let Some(reference) = account.token_ref {
                operation.state.pending.remove(&reference);
                operation.state.metadata.remove(&reference);
            }
            service.publish(&store, true);
            Ok(if service.cleanup(&store).await.unwrap_or(false) {
                RemovalOutcome::Removed
            } else {
                RemovalOutcome::CleanupPending
            })
        })
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
    }

    pub async fn resolve_launch(
        &self,
        id: &str,
        cancel: CancelToken,
    ) -> Result<LaunchGrant, AuthError> {
        let service = self.clone();
        let id = id.to_owned();
        check_cancel(&cancel)?;
        let (_cancel_on_drop, cancel, relay) = relay_cancel(cancel, None);
        tokio::spawn(async move {
            let _relay = relay;
            service.resolve_inner(&id, cancel).await
        })
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
    }
    async fn resolve_inner(&self, id: &str, cancel: CancelToken) -> Result<LaunchGrant, AuthError> {
        let mut operation = self.lock(cancel.clone()).await?;
        let store = self.load().await?;
        let mut store = self.recover(&mut operation, store).await?;
        self.publish(&store, false);
        let account = find(&store, id)?.clone();
        if account.kind == AccountKind::Offline {
            return offline_grant(&account, &store.accounts);
        }
        let reference = account
            .token_ref
            .clone()
            .ok_or(AuthError::ReauthenticationRequired)?;
        let record = self.read_record(reference.clone()).await?;
        if record.reauth_required() {
            return Err(AuthError::ReauthenticationRequired);
        }
        let now = self.inner.clock.unix_seconds();
        if let Some(session) = record.minecraft()
            && session.profile_uuid() == account.uuid
            && now >= session.issued_at()
            && now
                .checked_add(60)
                .is_some_and(|n| session.expires_at() > n)
        {
            check_cancel(&cancel)?;
            return Ok(grant(&account, session));
        }
        // Establish durable intent before any exchange can rotate the secret.
        self.journal(vec![reference.clone()]).await?;
        let tokens = match self
            .inner
            .client
            .refresh_microsoft(record.refresh_token().clone(), cancel.clone())
            .await
        {
            Ok(tokens) => tokens,
            Err(AuthError::ReauthenticationRequired) => {
                let record = CredentialRecord::new(record.refresh_token().clone(), None, true)
                    .map_err(super::credentials::write_error)?;
                operation
                    .state
                    .pending
                    .insert(reference.clone(), record.clone());
                self.write_record(reference.clone(), record).await?;
                operation.state.pending.remove(&reference);
                self.publish(&store, true);
                return Err(AuthError::ReauthenticationRequired);
            }
            Err(error) => return Err(error),
        };
        // No await/cancellation boundary between receiving rotation and retaining
        // its private retry copy; the owned operation outlives all public waiters.
        let rotated = CredentialRecord::new(tokens.refresh_token().clone(), None, false)
            .map_err(super::credentials::write_error)?;
        operation
            .state
            .pending
            .insert(reference.clone(), rotated.clone());
        self.write_record(reference.clone(), rotated).await?;
        operation.state.pending.remove(&reference);
        check_cancel(&cancel)?;
        if tokens.expires_at() <= self.inner.clock.unix_seconds() {
            return Err(AuthError::SessionExpired);
        }
        let verified = self
            .inner
            .client
            .complete_minecraft(tokens, cancel.clone())
            .await;
        let verified = match verified {
            Ok(login) if login.profile().uuid() == account.uuid => login,
            result => {
                let error = result.err().unwrap_or(AuthError::IdentityMismatch);
                if matches!(
                    error,
                    AuthError::NoEntitlement
                        | AuthError::ProfileMissing
                        | AuthError::IdentityMismatch
                ) {
                    operation.state.revoked.insert(reference.clone());
                    store
                        .accounts
                        .iter_mut()
                        .find(|a| a.id == id)
                        .expect("loaded identity")
                        .verification = None;
                    // Publish fail-closed policy even if durable revocation fails.
                    store.recompute_current();
                    self.publish(&store, true);
                    store = self.save(&mut operation, store).await?;
                    self.publish(&store, false);
                }
                return Err(error);
            }
        };
        let (profile, refresh, session, verified_at) = verified.into_parts();
        let record = CredentialRecord::new(refresh, Some(session.clone()), false)
            .map_err(super::credentials::write_error)?;
        operation
            .state
            .pending
            .insert(reference.clone(), record.clone());
        let account = store
            .accounts
            .iter_mut()
            .find(|a| a.id == id)
            .expect("loaded identity");
        account.username = profile.username().to_owned();
        account.skin_hash = profile.skin_hash().map(str::to_owned);
        account.verification = Some(Verification {
            profile_uuid: profile.uuid(),
            verified_at,
        });
        let account = account.clone();
        operation
            .state
            .metadata
            .insert(reference.clone(), account.clone());
        // The durable rotated record has no Minecraft cache. Commit its verified
        // identity/skin/proof before enabling the fresh cache, so loss of private
        // retry state can require another refresh but cannot grant stale identity.
        let store = self.save(&mut operation, store).await?;
        operation.state.metadata.remove(&reference);
        self.publish(&store, true);
        self.write_record(reference.clone(), record).await?;
        operation.state.pending.remove(&reference);
        check_cancel(&cancel)?;
        if self.inner.clock.unix_seconds() >= session.expires_at() {
            return Err(AuthError::SessionExpired);
        }
        Ok(grant(&account, &session))
    }
    async fn validate_grant(
        &self,
        store: &AccountStore,
        grant: &LaunchGrant,
    ) -> Result<(), AuthError> {
        let account = find(store, &grant.id)?;
        if account.uuid != grant.account.uuid()
            || account.username != grant.account.name()
            || account.token_ref != grant.reference
            || account.offline_origin() != grant.offline_origin
            || (account.kind == AccountKind::Msa)
                != (grant.account.user_type() == crate::launch::account::UserType::Msa)
        {
            return Err(AuthError::IdentityMismatch);
        }
        if account.kind == AccountKind::Offline {
            offline_grant(account, &store.accounts)?;
            return Ok(());
        }
        let now = self.inner.clock.unix_seconds();
        if now < grant.issued_at || now >= grant.expires_at {
            return Err(AuthError::SessionExpired);
        }
        let record = self
            .read_record(
                grant
                    .reference
                    .clone()
                    .ok_or(AuthError::ReauthenticationRequired)?,
            )
            .await?;
        if record.reauth_required() {
            return Err(AuthError::ReauthenticationRequired);
        }
        if !record.minecraft().is_some_and(|s| {
            s.profile_uuid() == account.uuid
                && s.issued_at() == grant.issued_at
                && s.expires_at() == grant.expires_at
                && s.xuid() == grant.account.xuid.as_deref()
                && s.access_token().expose() == grant.account.access_token
        }) {
            return Err(AuthError::SessionExpired);
        }
        // Keyring read can take time. Check again at the actual spawn boundary.
        if self.inner.clock.unix_seconds() < grant.issued_at
            || self.inner.clock.unix_seconds() >= grant.expires_at
        {
            return Err(AuthError::SessionExpired);
        }
        Ok(())
    }
    pub async fn launch_pending(
        &self,
        grant: LaunchGrant,
        pending: PendingLaunch,
        progress: ProgressSink,
    ) -> Result<GameSession, AuthenticatedLaunchError> {
        let service = self.clone();
        let (tx, cancel) = watch::channel(false);
        let _cancel_on_drop = CancelOnDrop(tx);
        tokio::spawn(async move {
            let mut operation = service
                .lock(cancel.clone())
                .await
                .map_err(AuthenticatedLaunchError::Auth)?;
            let store = service
                .load()
                .await
                .map_err(AuthenticatedLaunchError::Auth)?;
            let store = service
                .recover(&mut operation, store)
                .await
                .map_err(AuthenticatedLaunchError::Auth)?;
            service
                .validate_grant(&store, &grant)
                .await
                .map_err(AuthenticatedLaunchError::Auth)?;
            if !pending.matches_account(grant.account()) {
                return Err(AuthenticatedLaunchError::Auth(AuthError::IdentityMismatch));
            }
            check_cancel(&cancel).map_err(AuthenticatedLaunchError::Auth)?;
            pending
                .launch(progress)
                .await
                .map_err(AuthenticatedLaunchError::Session)
        })
        .await
        .map_err(|_| AuthenticatedLaunchError::Auth(AuthError::StorageUnavailable))?
    }
}
fn find<'a>(store: &'a AccountStore, id: &str) -> Result<&'a Account, AuthError> {
    store
        .accounts
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| AuthError::Account(super::AccountError::NotFound(id.to_owned())))
}
fn grant(account: &Account, session: &MinecraftSession) -> LaunchGrant {
    LaunchGrant {
        account: LaunchAccount::msa(
            &account.username,
            account.uuid,
            session.access_token().expose(),
            session.xuid().map(str::to_owned),
        ),
        id: account.id.clone(),
        reference: account.token_ref.clone(),
        offline_origin: None,
        issued_at: session.issued_at(),
        expires_at: session.expires_at(),
    }
}
fn offline_grant(account: &Account, accounts: &[Account]) -> Result<LaunchGrant, AuthError> {
    if !is_launchable(account, accounts) {
        return Err(AuthError::OfflineNotAllowed);
    }
    Ok(LaunchGrant {
        account: LaunchAccount::offline(&account.username, account.uuid),
        id: account.id.clone(),
        reference: account.token_ref.clone(),
        offline_origin: account.offline_origin(),
        issued_at: 0,
        expires_at: u64::MAX,
    })
}
impl DeviceCodeLogin for AccountService {
    fn start(
        &self,
        events: UnboundedSender<LoginEvent>,
        cancel: CancelToken,
    ) -> BoxFuture<'static, ()> {
        let service = self.clone();
        Box::pin(async move {
            // The worker retains events during commit. Closing the view before
            // commit cancels; closing after commit cannot suppress publication.
            let (_cancel_on_drop, cancel, relay) = relay_cancel(cancel, Some(events.clone()));
            let worker = tokio::spawn(async move {
                let _relay = relay;
                let result = match service
                    .inner
                    .client
                    .sign_in(events.clone(), cancel.clone())
                    .await
                {
                    Ok(login) => service.commit_login(login, cancel, Some(&events)).await,
                    Err(error) => Err(error),
                };
                let terminal = match result {
                    Ok((account, kind)) => {
                        let _ = events.send(LoginEvent::Storage(kind));
                        LoginEvent::Done {
                            username: account.username,
                            uuid: account.uuid,
                        }
                    }
                    Err(AuthError::Cancelled) => LoginEvent::Failed(LoginError::Cancelled),
                    Err(error) => LoginEvent::Failed(LoginError::Auth(error)),
                };
                let _ = events.send(terminal);
            });
            let _ = worker.await;
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use super::*;
    /// Synthetic verified fixture only. Production has no proof/token constructor.
    pub async fn seed_verified(
        service: &AccountService,
        name: &str,
        uuid: Uuid,
        refresh: &str,
        issued_at: u64,
        expires_at: u64,
    ) -> Result<Account, AuthError> {
        let login =
            super::super::msa::testing::verified(name, uuid, refresh, issued_at, expires_at)?;
        let service = service.clone();
        tokio::spawn(async move {
            let (_tx, cancel) = watch::channel(false);
            service
                .commit_login(login, cancel, None)
                .await
                .map(|(a, _)| a)
        })
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
    }
}
