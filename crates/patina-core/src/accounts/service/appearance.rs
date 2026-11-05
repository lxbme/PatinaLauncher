//! Authentication/rotation stays in the existing owned transaction. Decoration HTTP
//! runs outside exclusion, then revalidates the captured credential generation.
use super::*;
use crate::accounts::{
    appearance::{
        AppearanceError, AppearanceIdentity, AppearanceProfile, AppearanceTexture, parse_profile,
    },
    error::AuthStage,
    secret::SecretString,
};

impl AccountService {
    pub async fn appearance(
        &self,
        id: &str,
        cancel: CancelToken,
    ) -> Result<AppearanceProfile, AppearanceError> {
        check_cancel(&cancel)?;
        let (_guard, cancel, relay) = relay_cancel(cancel, None);
        let service = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let _relay = relay;
            let grant = service.resolve_inner(&id, cancel.clone()).await?;
            let identity = {
                let mut operation = service.lock(cancel.clone()).await?;
                let store = service.load().await?;
                let store = service.recover(&mut operation, store).await?;
                service.validate_grant(&store, &grant).await?;
                let account = find(&store, &id)?.clone();
                if account.kind != AccountKind::Msa {
                    return Err(AuthError::ReauthenticationRequired.into());
                }
                service.publish(&store, false);
                AppearanceIdentity {
                    account,
                    revision: *service.inner.changes.borrow(),
                }
            };
            let response = service
                .inner
                .client
                .appearance_profile(
                    SecretString::new(grant.account.access_token.clone()),
                    cancel.clone(),
                )
                .await?;
            if response.status() != 200 {
                return Err(AuthError::Http {
                    stage: AuthStage::Profile,
                    status: response.status(),
                }
                .into());
            }
            let profile = parse_profile(response.body(), identity)?;
            service
                .validate_appearance_identity(&profile.identity, cancel)
                .await?;
            Ok(profile)
        })
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
    }
    async fn validate_appearance_identity(
        &self,
        identity: &AppearanceIdentity,
        cancel: CancelToken,
    ) -> Result<(), AuthError> {
        let mut operation = self.lock(cancel.clone()).await?;
        let store = self.load().await?;
        let store = self.recover(&mut operation, store).await?;
        self.publish(&store, false);
        check_cancel(&cancel)?;
        if store.accounts.iter().find(|a| a.id == identity.account.id) != Some(&identity.account)
            || *self.inner.changes.borrow() != identity.revision
        {
            return Err(AuthError::IdentityMismatch);
        }
        Ok(())
    }
    /// Only opaque profile-derived identifiers are accepted; this client has no credentials.
    /// No cache is populated, and both sides of the wait validate the same target/revision.
    pub async fn appearance_texture(
        &self,
        id: &str,
        texture: &AppearanceTexture,
        cancel: CancelToken,
    ) -> Result<Vec<u8>, AppearanceError> {
        check_cancel(&cancel)?;
        if id != texture.identity.account.id {
            return Err(AuthError::IdentityMismatch.into());
        }
        self.validate_appearance_identity(&texture.identity, cancel.clone())
            .await?;
        let bytes = self
            .inner
            .skin
            .fetch(texture.hash(), cancel.clone())
            .await
            .ok_or(AuthError::Network {
                stage: AuthStage::Skin,
            })?;
        self.validate_appearance_identity(&texture.identity, cancel)
            .await?;
        Ok(bytes)
    }
}

use crate::accounts::appearance::{AppearanceChanges, ApplyOutcome, CapeChange, FieldOutcome};
struct AppearanceWritePermit {
    service: AccountService,
    id: String,
}
impl Drop for AppearanceWritePermit {
    fn drop(&mut self) {
        self.service
            .inner
            .appearance_writes
            .lock()
            .expect("appearance writes")
            .remove(&self.id);
    }
}
fn field_result(result: Result<super::super::http::AuthResponse, AuthError>) -> FieldOutcome {
    match result {
        Ok(response) if matches!(response.status(), 200 | 204) => FieldOutcome::Confirmed,
        Ok(response) => {
            let error = AppearanceError::from(AuthError::Http {
                stage: AuthStage::Profile,
                status: response.status(),
            });
            if (400..500).contains(&response.status()) {
                FieldOutcome::Failed(error)
            } else {
                FieldOutcome::Uncertain(error)
            }
        }
        Err(error) => FieldOutcome::Uncertain(error.into()),
    }
}
impl AccountService {
    /// Once a request may be sent, the owned worker retains its acknowledgement and
    /// publishes confirmed metadata even if its UI waiter disappears. Cancellation
    /// prevents subsequent writes, not the remote side effect already in progress.
    pub async fn apply_appearance(
        &self,
        id: &str,
        changes: AppearanceChanges,
        cancel: CancelToken,
    ) -> Result<ApplyOutcome, AppearanceError> {
        check_cancel(&cancel)?;
        if id != changes.baseline.account_id() {
            return Err(AuthError::IdentityMismatch.into());
        }
        {
            let mut pending = self
                .inner
                .appearance_writes
                .lock()
                .expect("appearance writes");
            if !pending.insert(id.to_owned()) {
                return Err(AuthError::Busy.into());
            }
        }
        let permit = AppearanceWritePermit {
            service: self.clone(),
            id: id.to_owned(),
        };
        let (_guard, cancel, relay) = relay_cancel(cancel, None);
        let service = self.clone();
        let id = id.to_owned();
        let uncertain_changes = changes.clone();
        match tokio::spawn(async move {
            let _permit = permit;
            let _relay = relay;
            service.apply_appearance_inner(&id, changes, cancel).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                // Losing the owned worker is not evidence that no request was sent.
                let error = AppearanceError::from(AuthError::StorageUnavailable);
                let skin = if uncertain_changes.skin.is_some() {
                    FieldOutcome::Uncertain(error.clone())
                } else {
                    FieldOutcome::NotRequested
                };
                let cape = if uncertain_changes.cape == CapeChange::Keep {
                    FieldOutcome::NotRequested
                } else {
                    FieldOutcome::Uncertain(error.clone())
                };
                Ok(ApplyOutcome {
                    changes: uncertain_changes,
                    skin,
                    cape,
                    profile: None,
                    refresh_error: Some(error),
                })
            }
        }
    }
    async fn apply_appearance_inner(
        &self,
        id: &str,
        mut changes: AppearanceChanges,
        cancel: CancelToken,
    ) -> Result<ApplyOutcome, AppearanceError> {
        // A launch-selection/other-account change does not change this editing target.
        // Still require the exact saved identity/credential reference, then obtain a
        // fresh profile and use its revision for every actual write boundary.
        {
            let mut operation = self.lock(cancel.clone()).await?;
            let store = self.load().await?;
            let store = self.recover(&mut operation, store).await?;
            if find(&store, id)? != &changes.baseline.identity.account {
                return Err(AuthError::IdentityMismatch.into());
            }
            check_cancel(&cancel)?;
        }
        let fresh = self.appearance(id, cancel.clone()).await?;
        if fresh.identity.account != changes.baseline.identity.account {
            return Err(AuthError::IdentityMismatch.into());
        }
        if let CapeChange::Select(id) = &changes.cape
            && !fresh.capes().iter().any(|c| c.id() == id)
        {
            return Err(AuthError::InvalidResponse {
                stage: AuthStage::Profile,
            }
            .into());
        }
        changes.baseline = fresh.clone();
        let grant = self.resolve_inner(id, cancel.clone()).await?;
        let mut out = ApplyOutcome {
            changes,
            skin: FieldOutcome::NotRequested,
            cape: FieldOutcome::NotRequested,
            profile: Some(fresh.clone()),
            refresh_error: None,
        };
        if let Some(skin) = &out.changes.skin {
            // This exclusion is intentional only for finite writes: deletion/relogin
            // cannot interleave between final credential validation and send.
            let validated = self
                .appearance_write_boundary(&grant, &fresh.identity, cancel.clone())
                .await;
            let (_operation, _store) = validated?;
            check_cancel(&cancel)?;
            let (_keep_alive, request_cancel) = watch::channel(false);
            out.skin = field_result(
                self.inner
                    .client
                    .upload_skin(
                        SecretString::new(grant.account.access_token.clone()),
                        skin.clone(),
                        request_cancel,
                    )
                    .await,
            );
        }
        if out.changes.skin.is_some() && out.skin != FieldOutcome::Confirmed {
            self.refresh_appearance_outcome(&mut out).await;
            return Ok(out);
        }
        let cape_id = match &out.changes.cape {
            CapeChange::Keep => None,
            CapeChange::Disable if fresh.active_cape().is_some() => Some(None),
            CapeChange::Select(id) if fresh.active_cape().map(|c| c.id()) != Some(id) => {
                Some(Some(id.as_str().to_owned()))
            }
            _ => None,
        };
        if let Some(cape_id) = cape_id {
            match self
                .appearance_write_boundary(&grant, &fresh.identity, cancel.clone())
                .await
            {
                Err(error) if out.skin == FieldOutcome::NotRequested => return Err(error.into()),
                Err(error) => out.cape = FieldOutcome::Failed(error.into()),
                Ok((_operation, _store)) => {
                    let (_keep_alive, request_cancel) = watch::channel(false);
                    out.cape = field_result(
                        self.inner
                            .client
                            .change_cape(
                                SecretString::new(grant.account.access_token.clone()),
                                cape_id,
                                request_cancel,
                            )
                            .await,
                    );
                }
            }
        }
        self.refresh_appearance_outcome(&mut out).await;
        Ok(out)
    }
    async fn appearance_write_boundary(
        &self,
        grant: &LaunchGrant,
        identity: &AppearanceIdentity,
        cancel: CancelToken,
    ) -> Result<(Operation, AccountStore), AuthError> {
        let mut operation = self.lock(cancel.clone()).await?;
        let store = self.load().await?;
        let store = self.recover(&mut operation, store).await?;
        self.publish(&store, false);
        self.validate_grant(&store, grant).await?;
        check_cancel(&cancel)?;
        if store.accounts.iter().find(|a| a.id == identity.account.id) != Some(&identity.account)
            || *self.inner.changes.borrow() != identity.revision
        {
            return Err(AuthError::IdentityMismatch);
        }
        Ok((operation, store))
    }
    async fn refresh_appearance_outcome(&self, out: &mut ApplyOutcome) {
        let (_tx, cancel) = watch::channel(false);
        let expected = out
            .profile
            .as_ref()
            .unwrap_or(&out.changes.baseline)
            .identity
            .clone();
        let result = async {
            self.validate_appearance_identity(&expected, cancel.clone())
                .await?;
            let mut profile = self
                .appearance(out.changes.baseline.account_id(), cancel.clone())
                .await?;
            if profile.identity != expected {
                return Err(AppearanceError::from(AuthError::IdentityMismatch));
            }
            if matches!(out.cape, FieldOutcome::Uncertain(_)) {
                let matches = match &out.changes.cape {
                    CapeChange::Select(id) => {
                        profile.active_cape().is_some_and(|cape| cape.id() == id)
                    }
                    CapeChange::Disable => profile.active_cape().is_none(),
                    CapeChange::Keep => false,
                };
                if matches {
                    out.cape = FieldOutcome::Confirmed;
                }
            }
            if out.skin == FieldOutcome::Confirmed {
                let mut operation = self.lock(cancel.clone()).await?;
                let store = self.load().await?;
                let mut store = self.recover(&mut operation, store).await?;
                if store.accounts.iter().find(|a| a.id == profile.account_id())
                    != Some(&profile.identity.account)
                {
                    return Err(AuthError::IdentityMismatch.into());
                }
                let account = store
                    .accounts
                    .iter_mut()
                    .find(|a| a.id == profile.account_id())
                    .expect("validated identity");
                let hash = profile.skin().map(|skin| skin.texture().hash().to_owned());
                let changed = account.skin_hash != hash;
                account.skin_hash = hash;
                let account = account.clone();
                if changed {
                    let store = self.save(&mut operation, store).await?;
                    self.publish(&store, true);
                }
                profile.rebind(AppearanceIdentity {
                    account,
                    revision: *self.inner.changes.borrow(),
                });
            }
            Ok::<_, AppearanceError>(profile)
        }
        .await;
        match result {
            Ok(profile) => {
                out.changes.baseline = profile.clone();
                out.profile = Some(profile);
                out.refresh_error = None;
            }
            Err(error) => {
                out.profile = None;
                out.refresh_error = Some(error);
            }
        }
    }
    /// One explicit bounded read; never replays a skin or cape mutation. An ambiguous
    /// skin stays ambiguous because server re-encoding invalidates raw-PNG hash proof.
    pub async fn reconcile_appearance(
        &self,
        outcome: &ApplyOutcome,
        cancel: CancelToken,
    ) -> Result<ApplyOutcome, AppearanceError> {
        check_cancel(&cancel)?;
        let mut out = outcome.clone();
        self.refresh_appearance_outcome(&mut out).await;
        check_cancel(&cancel)?;
        Ok(out)
    }
}
