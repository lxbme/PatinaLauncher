//! Network outside exclusion; all disk reads/publication revalidate under it.
use super::{
    Account, AccountKind, AccountService, AccountStore, AuthError, CancelToken, blocking,
    check_cancel, find, relay_cancel,
};
use crate::{
    accounts::skin::{decode_head, texture_url},
    platform::account_heads::HeadDirectory,
};
use std::{collections::HashSet, path::PathBuf};

fn filename(account: &Account) -> Option<String> {
    if account.kind != AccountKind::Msa {
        return None;
    }
    let hash = account.skin_hash()?;
    texture_url(hash)?;
    Some(format!("{}-{hash}.png", account.uuid.simple()))
}
fn optional<T>(result: Result<T, AuthError>) -> Result<Option<T>, AuthError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(AuthError::UnsafeCredentialPath) => Err(AuthError::UnsafeCredentialPath),
        Err(_) => Ok(None),
    }
}
fn normalize_cached_head(bytes: &[u8]) -> Option<Vec<u8>> {
    use image::{ImageDecoder, ImageFormat, Limits, codecs::png::PngDecoder};
    let mut limits = Limits::default();
    // Include DynamicImage's external RGBA16 output allocation.
    limits.max_alloc = Some(1024 * 1024 - 64 * 64 * 8);
    limits.max_image_width = Some(64);
    limits.max_image_height = Some(64);
    let decoder = PngDecoder::with_limits(std::io::Cursor::new(bytes), limits).ok()?;
    if decoder.dimensions() != (64, 64) {
        return None;
    }
    let pixels = image::DynamicImage::from_decoder(decoder)
        .ok()?
        .into_rgba8();
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(pixels)
        .write_to(&mut output, ImageFormat::Png)
        .ok()?;
    Some(output.into_inner())
}

impl AccountService {
    /// Loopback-only injected skin transport, before sharing the test service.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_skin_endpoint_for_test(&mut self, endpoint: &str) -> Result<(), AuthError> {
        let skin = crate::accounts::skin::SkinClient::for_test(endpoint).ok_or(
            AuthError::InvalidResponse {
                stage: crate::accounts::error::AuthStage::Skin,
            },
        )?;
        std::sync::Arc::get_mut(&mut self.inner)
            .ok_or(AuthError::Busy)?
            .skin = skin;
        Ok(())
    }
    /// Optional, normalized head cache. The returned path is not authority to
    /// open it later: use `read_cached_head` to revalidate current identity/path.
    pub async fn cache_head(
        &self,
        account_id: &str,
        cancel: CancelToken,
    ) -> Result<Option<PathBuf>, AuthError> {
        check_cancel(&cancel)?;
        let (_guard, cancel, relay) = relay_cancel(cancel, None);
        let service = self.clone();
        let id = account_id.to_owned();
        tokio::spawn(async move {
            let _relay = relay;
            let (account, name, revision) = {
                let mut operation = service.lock(cancel.clone()).await?;
                let store = service.load().await?;
                let store = service.recover(&mut operation, store).await?;
                service.publish(&store, false);
                let account = find(&store, &id)?.clone();
                let Some(name) = filename(&account) else {
                    return Ok(None);
                };
                let revision = *service.inner.changes.borrow();
                let data = service.inner.data.clone();
                let read_name = name.clone();
                let cached =
                    optional(blocking(move || HeadDirectory::open(&data)?.read(&read_name)).await)?
                        .flatten();
                check_cancel(&cancel)?;
                if let Some(bytes) = cached {
                    let valid =
                        blocking(move || Ok(normalize_cached_head(&bytes).is_some())).await?;
                    return Ok(
                        valid.then(|| service.inner.data.cache().join("account-heads").join(name))
                    );
                }
                (account, name, revision)
            };
            let Some(bytes) = service
                .inner
                .skin
                .fetch(account.skin_hash().expect("validated hash"), cancel.clone())
                .await
            else {
                check_cancel(&cancel)?;
                return Ok(None);
            };
            let Some(png) = blocking(move || Ok(decode_head(&bytes).ok())).await? else {
                return Ok(None);
            };
            let mut operation = service.lock(cancel.clone()).await?;
            let store = service.load().await?;
            let store = service.recover(&mut operation, store).await?;
            service.publish(&store, false);
            // Reference replacement covers re-login (including same hash). The
            // watch revision also rejects same-owner ABA and coalesced mutations.
            if store.accounts.iter().find(|a| a.id == id) != Some(&account)
                || *service.inner.changes.borrow() != revision
            {
                return Ok(None);
            }
            check_cancel(&cancel)?;
            let data = service.inner.data.clone();
            let path = data.cache().join("account-heads").join(&name);
            let wrote =
                optional(blocking(move || HeadDirectory::open(&data)?.write(&name, &png)).await)?;
            check_cancel(&cancel)?;
            Ok(wrote.map(|()| path))
        })
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
    }
    /// Read only the current account's bounded normalized PNG under exclusion.
    /// UI consumers must additionally reject completion if their captured watch
    /// revision/identity changed before publication (including coalesced wakes).
    pub async fn read_cached_head(
        &self,
        account_id: &str,
        cancel: CancelToken,
    ) -> Result<Option<Vec<u8>>, AuthError> {
        check_cancel(&cancel)?;
        let (_guard, cancel, relay) = relay_cancel(cancel, None);
        let service = self.clone();
        let id = account_id.to_owned();
        tokio::spawn(async move {
            let _relay = relay;
            let mut operation = service.lock(cancel.clone()).await?;
            let store = service.load().await?;
            let store = service.recover(&mut operation, store).await?;
            service.publish(&store, false);
            let Some(account) = store.accounts.iter().find(|a| a.id == id) else {
                return Ok(None);
            };
            let Some(name) = filename(account) else {
                return Ok(None);
            };
            let data = service.inner.data.clone();
            let bytes = optional(
                blocking(move || {
                    let bytes = HeadDirectory::open(&data)?.read(&name)?;
                    Ok(bytes.and_then(|b| normalize_cached_head(&b)))
                })
                .await,
            )?
            .flatten();
            check_cancel(&cancel)?;
            Ok(bytes)
        })
        .await
        .map_err(|_| AuthError::StorageUnavailable)?
    }
    pub(super) async fn cleanup_heads(&self, store: &AccountStore) -> Result<(), AuthError> {
        let active: HashSet<_> = store
            .accounts
            .iter()
            .chain(&store.hidden)
            .filter_map(filename)
            .collect();
        let data = self.inner.data.clone();
        blocking(move || HeadDirectory::open(&data)?.retain(&active)).await
    }
}
