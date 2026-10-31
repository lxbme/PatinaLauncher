//! Network outside exclusion; all disk reads/publication revalidate under it.
use super::{
    Account, AccountKind, AccountService, AccountStore, AuthError, CancelToken, blocking,
    cancelled, check_cancel, find, relay_cancel,
};
use crate::{
    accounts::skin::{decode_head, decode_pixels, texture_url},
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
    use image::ImageFormat;
    let pixels = decode_pixels(bytes).ok()?;
    if pixels.dimensions() != (64, 64) {
        return None;
    }
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(pixels)
        .write_to(&mut output, ImageFormat::Png)
        .ok()?;
    Some(output.into_inner())
}

impl AccountService {
    async fn decode_skin<T: Send + 'static>(
        &self,
        mut cancel: CancelToken,
        decode: impl FnOnce() -> Result<T, AuthError> + Send + 'static,
    ) -> Result<T, AuthError> {
        check_cancel(&cancel)?;
        #[cfg(test)]
        let probe = self
            .inner
            .skin_decode_probe
            .lock()
            .expect("decode probe")
            .clone();
        #[cfg(test)]
        if let Some(probe) = &probe {
            probe.attempt();
        }
        // The public waiter/app bridge can disappear while spawn_blocking is
        // still running. Only the actual closure may release this admission.
        let permit = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return Err(AuthError::Cancelled),
            permit = self.inner.skin_decodes.clone().acquire_owned() =>
                permit.map_err(|_| AuthError::StorageUnavailable)?,
        };
        blocking(move || {
            let _permit = permit;
            check_cancel(&cancel)?;
            #[cfg(test)]
            let _entry = probe.as_ref().map(|probe| probe.enter());
            let result = decode();
            check_cancel(&cancel)?;
            result
        })
        .await
    }
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
                    let valid = service
                        .decode_skin(cancel.clone(), move || {
                            Ok(normalize_cached_head(&bytes).is_some())
                        })
                        .await?;
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
            let Some(png) = service
                .decode_skin(cancel.clone(), move || Ok(decode_head(&bytes).ok()))
                .await?
            else {
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
                service
                    .decode_skin(cancel.clone(), move || {
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

// A per-service barrier observes real closure entry, not an app-side mock.
#[cfg(test)]
#[derive(Default)]
pub(super) struct DecodeProbe {
    attempts: std::sync::atomic::AtomicUsize,
    entries: std::sync::atomic::AtomicUsize,
    active: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    released: std::sync::Mutex<bool>,
    gate: std::sync::Condvar,
    changed: tokio::sync::Notify,
}
#[cfg(test)]
impl DecodeProbe {
    fn attempt(&self) {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.changed.notify_one();
    }
    fn enter(&self) -> DecodeEntry<'_> {
        use std::sync::atomic::Ordering::SeqCst;
        let active = self.active.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(active, SeqCst);
        let first = self.entries.fetch_add(1, SeqCst) == 0;
        self.changed.notify_one();
        if first {
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.gate.wait(released).unwrap();
            }
        }
        DecodeEntry(self)
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.gate.notify_all();
    }
}
#[cfg(test)]
struct DecodeEntry<'a>(&'a DecodeProbe);
#[cfg(test)]
impl Drop for DecodeEntry<'_> {
    fn drop(&mut self) {
        self.0
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::service_tests::{Fixture, ID};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };
    use tokio::sync::watch;
    use wiremock::{Mock, ResponseTemplate, matchers::path};
    struct Release(Arc<DecodeProbe>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    async fn reached(probe: &DecodeProbe, counter: &AtomicUsize, count: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while counter.load(SeqCst) < count {
                probe.changed.notified().await;
            }
        })
        .await
        .expect("decoder reached barrier");
    }
    async fn fixture() -> Fixture {
        let mut f = Fixture::new().await;
        f.service
            .set_skin_endpoint_for_test(&f.server.uri())
            .unwrap();
        f.chain("Player_123").await;
        f.login().await;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(64, 64))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        Mock::given(path("/texture/abcdef"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png.into_inner()))
            .mount(&f.server)
            .await;
        f
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_network_decode_keeps_admission_until_blocking_closure_exits() {
        let f = fixture().await;
        let probe = Arc::new(DecodeProbe::default());
        *f.service.inner.skin_decode_probe.lock().unwrap() = Some(probe.clone());
        let _release = Release(probe.clone());
        let (_tx, token) = watch::channel(false);
        let service = f.service.clone();
        let first_token = token.clone();
        let first = tokio::spawn(async move { service.cache_head(ID, first_token).await });
        reached(&probe, &probe.entries, 1).await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let service = f.service.clone();
        let next_token = token.clone();
        let next = tokio::spawn(async move { service.cache_head(ID, next_token).await });
        reached(&probe, &probe.attempts, 2).await;
        assert_eq!(
            f.service.inner.skin_decodes.available_permits(),
            0,
            "actual blocking decode lost its admission permit"
        );
        assert_eq!(probe.entries.load(SeqCst), 1);
        let (cancel, queued_token) = watch::channel(false);
        let service = f.service.clone();
        let queued = tokio::spawn(async move { service.cache_head(ID, queued_token).await });
        reached(&probe, &probe.attempts, 3).await;
        cancel.send(true).unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(3), queued)
                .await
                .unwrap()
                .unwrap(),
            Err(AuthError::Cancelled)
        ));
        assert_eq!(probe.entries.load(SeqCst), 1);
        assert!(!next.is_finished());
        probe.release();
        assert!(next.await.unwrap().unwrap().is_some());
        assert_eq!(probe.entries.load(SeqCst), 2);
        assert_eq!(probe.peak.load(SeqCst), 1);
        assert_eq!(f.service.inner.skin_decodes.available_permits(), 1);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_admission_future_cannot_release_a_running_blocking_job() {
        let f = Fixture::new().await;
        let probe = Arc::new(DecodeProbe::default());
        *f.service.inner.skin_decode_probe.lock().unwrap() = Some(probe.clone());
        let _release = Release(probe.clone());
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(64, 64))
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        let first_bytes = bytes.clone();
        let (_tx, token) = watch::channel(false);
        let first_token = token.clone();
        let service = f.service.clone();
        let first = tokio::spawn(async move {
            service
                .decode_skin(first_token, move || Ok(normalize_cached_head(&first_bytes)))
                .await
        });
        reached(&probe, &probe.entries, 1).await;
        // Unlike the public cancel-on-drop waiter, abort this owning async
        // admission future itself. Its spawn_blocking closure must retain admission.
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let service = f.service.clone();
        let next = tokio::spawn(async move {
            service
                .decode_skin(token, move || Ok(normalize_cached_head(&bytes)))
                .await
        });
        reached(&probe, &probe.attempts, 2).await;
        assert_eq!(
            f.service.inner.skin_decodes.available_permits(),
            0,
            "permit lived in the aborted future instead of the closure"
        );
        assert_eq!(probe.entries.load(SeqCst), 1);
        assert!(!next.is_finished());
        probe.release();
        assert!(next.await.unwrap().unwrap().is_some());
        assert_eq!(probe.entries.load(SeqCst), 2);
        assert_eq!(probe.peak.load(SeqCst), 1);
        assert_eq!(f.service.inner.skin_decodes.available_permits(), 1);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_cached_normalization_keeps_admission_until_closure_exits() {
        // Cover both the cache-hit validator and the account-scoped reader.
        for reader in [false, true] {
            let f = fixture().await;
            let (_tx, token) = watch::channel(false);
            f.service
                .cache_head(ID, token.clone())
                .await
                .unwrap()
                .unwrap();
            let probe = Arc::new(DecodeProbe::default());
            *f.service.inner.skin_decode_probe.lock().unwrap() = Some(probe.clone());
            let _release = Release(probe.clone());
            let service = f.service.clone();
            let first_token = token.clone();
            let first = tokio::spawn(async move {
                if reader {
                    service
                        .read_cached_head(ID, first_token)
                        .await
                        .map(|v| v.is_some())
                } else {
                    service
                        .cache_head(ID, first_token)
                        .await
                        .map(|v| v.is_some())
                }
            });
            reached(&probe, &probe.entries, 1).await;
            first.abort();
            assert!(first.await.unwrap_err().is_cancelled());
            assert_eq!(
                f.service.inner.skin_decodes.available_permits(),
                0,
                "cached normalization lost its permit"
            );
            let service = f.service.clone();
            let next = tokio::spawn(async move { service.read_cached_head(ID, token).await });
            probe.release();
            assert!(next.await.unwrap().unwrap().is_some());
            assert_eq!(probe.entries.load(SeqCst), 2);
            assert_eq!(probe.peak.load(SeqCst), 1);
            assert_eq!(f.service.inner.skin_decodes.available_permits(), 1);
        }
    }
}
