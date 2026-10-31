//! One subscriber follows the service's monotonic revision, including coalesced changes.
use super::{Core, run};
use gpui_kit::{App, AppContext, Entity, Global, Task};

#[derive(Debug, Default)]
pub struct AccountsEpoch(u64);
impl AccountsEpoch {
    pub fn get(&self) -> u64 {
        self.0
    }
}

pub struct AccountsChanged {
    epoch: Entity<AccountsEpoch>,
    follow: Option<Task<()>>,
}
impl Global for AccountsChanged {}
impl AccountsChanged {
    pub fn init(cx: &mut App) {
        if !cx.has_global::<Self>() {
            let epoch = cx.new(|_| AccountsEpoch::default());
            cx.set_global(Self {
                epoch,
                follow: None,
            });
        }
    }
    pub fn epoch(cx: &App) -> Entity<AccountsEpoch> {
        cx.global::<Self>().epoch.clone()
    }

    /// Installed once after Core. This subscription also owns decoration loading;
    /// no view or render callback starts another watch or filesystem operation.
    pub fn follow_core(cx: &mut App) {
        Self::init(cx);
        if cx.global::<Self>().follow.is_some() {
            return;
        }
        let service = Core::global(cx).accounts();
        let mut changes = service.changes();
        let images = crate::accounts::avatar::install(cx);
        let task = cx.spawn(async move |cx| {
            loop {
                let observed = *changes.borrow_and_update();
                let read_service = service.clone();
                let snapshot = cx
                    .update(|cx| run(cx, async move { read_service.snapshot().await }))
                    .await;
                #[cfg(any(test, feature = "test-support"))]
                if let Some(released) = cx.update(|cx| {
                    super::testing::account_snapshot_delivery(
                        cx,
                        observed,
                        snapshot.as_ref().is_ok_and(Result::is_err),
                    )
                }) {
                    let _ = released.await;
                }
                let latest = *changes.borrow();
                // Errors carry no revision: never attribute a superseded read to
                // the latest producer state, even when that read failed.
                if observed != latest {
                    continue;
                }
                cx.update(|cx| {
                    let accounts = snapshot.as_ref().ok().and_then(|s| s.as_ref().ok());
                    // Never reconcile an obsolete successful read under a newer revision.
                    if accounts.is_some_and(|s| s.revision() != latest) {
                        return;
                    }
                    images.update(cx, |cache, cx| {
                        cache.reconcile(latest, accounts.map_or(&[], |s| s.accounts()), cx);
                        if let Some(snapshot) = accounts {
                            for account in snapshot.accounts() {
                                cache.load(account, service.as_ref().clone(), cx);
                            }
                        }
                    });
                    Self::epoch(cx).update(cx, |epoch, cx| {
                        if latest > epoch.0 {
                            epoch.0 = latest;
                            cx.notify();
                        }
                    });
                });
                if changes.changed().await.is_err() {
                    break;
                }
            }
        });
        cx.global_mut::<Self>().follow = Some(task);
    }
}
