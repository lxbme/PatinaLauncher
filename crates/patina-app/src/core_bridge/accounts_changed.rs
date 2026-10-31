//! The "accounts changed" signal: views that show account data observe it,
//! and every code path that changes the accounts bumps it.

use gpui_kit::{App, AppContext, Context, Entity, Global};

/// A counter that goes up whenever the accounts change (an entity, so views
/// can `cx.observe` it). Reach it through [`AccountsChanged`].
#[derive(Debug, Default)]
pub struct AccountsEpoch(u64);

impl AccountsEpoch {
    /// How many changes have been signalled so far.
    pub fn get(&self) -> u64 {
        self.0
    }

    fn bump(&mut self, cx: &mut Context<Self>) {
        self.0 += 1;
        cx.notify();
    }
}

/// The app's [`AccountsEpoch`] (a gpui `Global`, set by [`AccountsChanged::init`]).
///
/// **Every change of the accounts is followed by [`AccountsChanged::bump`]**
/// (add, remove, switch, sign-in, reload): views read the accounts again
/// only when told. Writers go through
/// [`Core::update_accounts`](super::Core::update_accounts) and
/// [`Core::reload_accounts`](super::Core::reload_accounts), which bump once
/// the change has finished.
pub struct AccountsChanged(Entity<AccountsEpoch>);

impl Global for AccountsChanged {}

impl AccountsChanged {
    /// Creates the signal, once per app. Call before opening windows.
    pub fn init(cx: &mut App) {
        if !cx.has_global::<Self>() {
            let epoch = cx.new(|_| AccountsEpoch::default());
            cx.set_global(Self(epoch));
        }
    }

    /// The entity to observe.
    pub fn epoch(cx: &App) -> Entity<AccountsEpoch> {
        cx.global::<Self>().0.clone()
    }

    /// Signals that the accounts changed; observers re-read them.
    pub fn bump(cx: &mut App) {
        Self::epoch(cx).update(cx, |e, cx| e.bump(cx));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::TestAppContext;

    use super::*;

    #[gpui_kit::test]
    fn bump_notifies_observers(cx: &mut TestAppContext) {
        cx.update(AccountsChanged::init);
        let seen = Rc::new(Cell::new(0));
        let _observe = cx.update(|cx| {
            let seen = seen.clone();
            cx.observe(&AccountsChanged::epoch(cx), move |_, _| {
                seen.set(seen.get() + 1)
            })
        });
        cx.update(AccountsChanged::bump);
        cx.update(AccountsChanged::bump);
        cx.run_until_parked();
        assert_eq!(seen.get(), 2);
        assert_eq!(cx.update(|cx| AccountsChanged::epoch(cx).read(cx).get()), 2);
        // A second init keeps the same signal.
        cx.update(AccountsChanged::init);
        assert_eq!(cx.update(|cx| AccountsChanged::epoch(cx).read(cx).get()), 2);
    }
}
