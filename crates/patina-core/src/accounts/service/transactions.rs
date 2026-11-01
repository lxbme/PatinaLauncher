//! Lock-held durable IO and journal reconciliation. No network polling here.
use super::{
    Account, AccountService, AccountStore, AuthError, CancelToken, CredentialRecord, LoginEvent,
    Operation, StorageKind, UnboundedSender, Uuid, Verification, VerifiedLogin, blocking,
    cancelled, check_cancel,
};
use std::time::Duration;

impl AccountService {
    pub(super) async fn lock(&self, mut cancel: CancelToken) -> Result<Operation, AuthError> {
        check_cancel(&cancel)?;
        let mut state = tokio::select! { biased; _ = cancelled(&mut cancel) => return Err(AuthError::Cancelled), state = self.inner.operation.clone().lock_owned() => state };
        if let Some(lock) = state.retained_lock.take() {
            return Ok(Operation {
                state,
                lock: Some(lock),
            });
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            check_cancel(&cancel)?;
            let credentials = self.inner.credentials.clone();
            let lock = blocking(move || credentials.try_lock()).await?;
            check_cancel(&cancel)?;
            if lock.is_some() {
                return Ok(Operation { state, lock });
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AuthError::Busy);
            }
            tokio::select! { biased; _ = cancelled(&mut cancel) => return Err(AuthError::Cancelled), _ = tokio::time::sleep_until((tokio::time::Instant::now() + Duration::from_millis(100)).min(deadline)) => {} }
        }
    }
    pub(super) async fn load(&self) -> Result<AccountStore, AuthError> {
        let data = self.inner.data.clone();
        match blocking(move || AccountStore::load_durable(&data)).await {
            Ok(mut store) => {
                store.recompute_current();
                Ok(store)
            }
            Err(error) => {
                let mut published = self.inner.snapshot.write().expect("account snapshot");
                if published.as_ref() != Some(&Err(error.clone())) {
                    *published = Some(Err(error.clone()));
                    let revision = self
                        .inner
                        .changes
                        .borrow()
                        .checked_add(1)
                        .expect("account revision overflow");
                    self.inner.changes.send_replace(revision);
                }
                Err(error)
            }
        }
    }
    pub(super) async fn save(
        &self,
        operation: &mut Operation,
        mut store: AccountStore,
    ) -> Result<AccountStore, AuthError> {
        // Every writer merges fail-closed state, including remove/login which
        // deliberately bypass broken secret-save recovery. Also sanitize queued
        // metadata so a later retry cannot restore proof after retiring markers.
        let state = &mut *operation.state;
        for account in store
            .accounts
            .iter_mut()
            .chain(&mut store.hidden)
            .chain(state.metadata.values_mut())
        {
            if account
                .token_ref
                .as_ref()
                .is_some_and(|r| state.revoked.contains(r))
            {
                account.verification = None;
            }
        }
        store.recompute_current();
        let data = self.inner.data.clone();
        #[cfg(test)]
        let fail = self
            .inner
            .metadata_fault
            .lock()
            .expect("metadata fault")
            .take();
        let store = blocking(move || {
            store.save_durable(
                &data,
                #[cfg(test)]
                fail,
            )?;
            Ok(store)
        })
        .await?;
        // All surviving references are now durably cleared; absent references
        // were durably removed/replaced. Errors above retain every marker.
        operation.state.revoked.clear();
        // Decoration cleanup follows metadata commit and cannot undo it.
        let _ = self.cleanup_heads(&store).await;
        Ok(store)
    }
    pub(super) async fn read_record(
        &self,
        reference: String,
    ) -> Result<CredentialRecord, AuthError> {
        let credentials = self.inner.credentials.clone();
        blocking(move || credentials.read(&reference)).await
    }
    pub(super) async fn write_record(
        &self,
        reference: String,
        record: CredentialRecord,
    ) -> Result<StorageKind, AuthError> {
        let credentials = self.inner.credentials.clone();
        blocking(move || credentials.write(&reference, &record)).await
    }
    pub(super) async fn journal(&self, refs: Vec<String>) -> Result<(), AuthError> {
        let credentials = self.inner.credentials.clone();
        blocking(move || {
            let mut journal = credentials.journal()?;
            for r in refs {
                if !journal.contains(&r) {
                    journal.push(r);
                }
            }
            credentials.write_journal(&journal)
        })
        .await
    }
    pub(super) async fn cleanup(&self, store: &AccountStore) -> Result<bool, AuthError> {
        // Run independently before native credential cleanup: its failure must
        // never preserve a removed account's head. Recovery retries both.
        let heads_clean = self.cleanup_heads(store).await.is_ok();
        let active = store.references();
        let credentials = self.inner.credentials.clone();
        blocking(move || {
            let journal = credentials.journal()?;
            let mut remaining = Vec::new();
            for reference in journal {
                if !active.contains(&reference) && credentials.delete(&reference).is_err() {
                    remaining.push(reference);
                }
            }
            credentials.write_journal(&remaining)?;
            Ok(remaining.is_empty() && heads_clean)
        })
        .await
    }
    pub(super) async fn recover(
        &self,
        operation: &mut Operation,
        mut store: AccountStore,
    ) -> Result<AccountStore, AuthError> {
        let refs = store.references();
        operation.state.pending.retain(|r, _| refs.contains(r));
        operation.state.revoked.retain(|r| refs.contains(r));
        operation.state.metadata.retain(|r, _| refs.contains(r));
        if !operation.state.metadata.is_empty() {
            for account in &mut store.accounts {
                if let Some(updated) = account
                    .token_ref
                    .as_ref()
                    .and_then(|r| operation.state.metadata.get(r))
                {
                    *account = updated.clone();
                }
            }
            store = self.save(operation, store).await?;
            operation.state.metadata.clear();
            self.publish(&store, true);
        }
        // Same ordering as refresh commit: never persist a fresh cache before
        // its associated metadata, including storage-only retry after failure.
        for (reference, record) in operation.state.pending.clone() {
            self.write_record(reference.clone(), record).await?;
            operation.state.pending.remove(&reference);
        }
        if !operation.state.revoked.is_empty() {
            store = self.save(operation, store).await?;
            // Proof was already published fail-closed. Durability alone is not
            // another public account change.
            self.publish(&store, false);
        }
        // Deferred deletion is retried on every explicit operation. An unavailable
        // backend never prevents a referenced account from being read or used.
        self.cleanup(&store).await?;
        Ok(store)
    }
    pub(super) async fn commit_login(
        &self,
        login: VerifiedLogin,
        cancel: CancelToken,
        events: Option<&UnboundedSender<LoginEvent>>,
    ) -> Result<(Account, StorageKind), AuthError> {
        let mut operation = self.lock(cancel.clone()).await?;
        let mut store = self.load().await?;
        check_cancel(&cancel)?;
        if events.is_some_and(|e| e.is_closed()) {
            return Err(AuthError::Cancelled);
        }
        let (profile, refresh, session, verified_at) = login.into_parts();
        if self.inner.clock.unix_seconds() >= session.expires_at() {
            return Err(AuthError::SessionExpired);
        }
        let id = profile.uuid().simple().to_string();
        let old = store
            .accounts
            .iter()
            .find(|a| a.id == id)
            .and_then(|a| a.token_ref.clone());
        let reference = Uuid::new_v4().to_string();
        self.journal(
            std::iter::once(reference.clone())
                .chain(old.clone())
                .collect(),
        )
        .await?;
        let record = CredentialRecord::new(refresh, Some(session), false)
            .map_err(super::super::credentials::write_error)?;
        let kind = match self.write_record(reference.clone(), record).await {
            Ok(kind) => kind,
            Err(error) => {
                let _ = self.cleanup(&store).await;
                return Err(error);
            }
        };
        if check_cancel(&cancel).is_err() || events.is_some_and(|e| e.is_closed()) {
            let _ = self.cleanup(&store).await;
            return Err(AuthError::Cancelled);
        }
        store.add_msa(profile.username(), profile.uuid(), Some(reference));
        let account = store
            .accounts
            .iter_mut()
            .find(|a| a.id == id)
            .expect("inserted identity");
        account.verification = Some(Verification {
            profile_uuid: profile.uuid(),
            verified_at,
        });
        account.skin_hash = profile.skin_hash().map(str::to_owned);
        let account = account.clone();
        store.set_current(&id).map_err(AuthError::Account)?;
        // After replacement starts, cancellation no longer rolls back. On any
        // save error leave the journal intact: replacement may already be visible.
        let store = self.save(&mut operation, store).await?;
        if let Some(old) = old {
            operation.state.pending.remove(&old);
            operation.state.metadata.remove(&old);
        }
        self.publish(&store, true);
        let _ = self.cleanup(&store).await;
        Ok((account, kind))
    }
}
