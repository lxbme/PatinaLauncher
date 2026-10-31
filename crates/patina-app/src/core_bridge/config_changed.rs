//! The "launcher settings changed" signal: views that show what the config,
//! the download source or the managed runtimes say (the status bar)
//! observe it, and the code paths that change them bump it.

use gpui_kit::{App, AppContext, Context, Entity, Global};

/// A counter that goes up whenever the launcher's settings change (an
/// entity, so views can `cx.observe` it). Reach it through
/// [`ConfigChanged`].
#[derive(Debug, Default)]
pub struct ConfigEpoch(u64);

impl ConfigEpoch {
    /// How many changes have been signalled so far.
    pub fn get(&self) -> u64 {
        self.0
    }

    fn bump(&mut self, cx: &mut Context<Self>) {
        self.0 += 1;
        cx.notify();
    }
}

/// The app's [`ConfigEpoch`] (a gpui `Global`, set by
/// [`ConfigChanged::init`]).
///
/// Bumped after [`Core::update_config`](super::Core::update_config) and
/// [`Core::speed_test_now`](super::Core::speed_test_now) finish, and after
/// a managed runtime is removed.
pub struct ConfigChanged(Entity<ConfigEpoch>);

impl Global for ConfigChanged {}

impl ConfigChanged {
    /// Creates the signal, once per app. Call before opening windows.
    pub fn init(cx: &mut App) {
        if !cx.has_global::<Self>() {
            let epoch = cx.new(|_| ConfigEpoch::default());
            cx.set_global(Self(epoch));
        }
    }

    /// The entity to observe.
    pub fn epoch(cx: &mut App) -> Entity<ConfigEpoch> {
        Self::init(cx);
        cx.global::<Self>().0.clone()
    }

    /// Signals that the settings changed; observers re-read them.
    pub fn bump(cx: &mut App) {
        Self::epoch(cx).update(cx, |e, cx| e.bump(cx));
    }

    /// Bumps the signal whenever the [`Core`](super::Core) records a speed
    /// test it ran on its own (the startup test), so the status bar shows
    /// it without waiting for the window to be activated. Call once, with
    /// the `Core` global set.
    pub fn follow_core(cx: &mut App) {
        Self::init(cx);
        let mut tests = super::Core::global(cx).speed_tests();
        cx.spawn(async move |cx| {
            while tests.changed().await.is_ok() {
                cx.update(Self::bump);
            }
        })
        .detach();
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
        let seen = Rc::new(Cell::new(0));
        let _observe = cx.update(|cx| {
            let seen = seen.clone();
            let epoch = ConfigChanged::epoch(cx);
            cx.observe(&epoch, move |_, _| seen.set(seen.get() + 1))
        });
        cx.update(ConfigChanged::bump);
        cx.update(ConfigChanged::bump);
        cx.run_until_parked();
        assert_eq!(seen.get(), 2);
        assert_eq!(cx.update(|cx| ConfigChanged::epoch(cx).read(cx).get()), 2);
    }
}
