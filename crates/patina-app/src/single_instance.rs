//! One Patina per data dir: an exclusive lock on `<data>/.patina-app.lock`,
//! held for the life of the process. A second process reports the first one
//! in a small window ([`open_notice`]) and quits.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Context, Entity, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, Styled, TitlebarOptions,
    Window, WindowBounds, WindowOptions, actions, div, size,
};
use patina_core::paths::DataDirs;
use rust_i18n::t;

use crate::fonts::Strong;

pub const LOCK_FILE: &str = ".patina-app.lock";

/// Proof that this process is the only Patina on its data dir; keep it alive
/// until the app exits (dropping it releases the lock).
#[derive(Debug)]
pub struct SingleInstance {
    _file: Option<File>,
    unguarded: Option<io::Error>,
}

impl SingleInstance {
    /// Why the lock could not be taken (the data dir is not writable, the
    /// file system has no locks), so this process runs without the guard.
    /// `None` when the lock is held.
    pub fn unguarded_reason(&self) -> Option<&io::Error> {
        self.unguarded.as_ref()
    }
}

/// Another Patina process holds the lock on this data dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another Patina process is using this data directory")
    }
}

impl std::error::Error for AlreadyRunning {}

/// Takes the lock on `<data>/.patina-app.lock` (creating the data dir).
///
/// An I/O problem that leaves the lock unknowable does not stop the app:
/// it gets an unguarded [`SingleInstance`] that says why.
pub fn acquire(data: &DataDirs) -> Result<SingleInstance, AlreadyRunning> {
    acquire_in(&data.root())
}

fn acquire_in(dir: &Path) -> Result<SingleInstance, AlreadyRunning> {
    let unguarded = |e| {
        Ok(SingleInstance {
            _file: None,
            unguarded: Some(e),
        })
    };
    let opened = fs::create_dir_all(dir).and_then(|()| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK_FILE))
    });
    let file = match opened {
        Ok(file) => file,
        Err(e) => return unguarded(e),
    };
    match file.try_lock() {
        Ok(()) => Ok(SingleInstance {
            _file: Some(file),
            unguarded: None,
        }),
        Err(TryLockError::WouldBlock) => Err(AlreadyRunning),
        Err(TryLockError::Error(e)) => unguarded(e),
    }
}

actions!(single_instance, [Dismiss]);

/// Key context of the [`AlreadyRunningNotice`] window.
const NOTICE_CONTEXT: &str = "AlreadyRunningNotice";

/// The second instance's window: the first instance is running, and a
/// button that closes it. It takes focus when it opens, so Enter and Escape
/// close it too. Closing the last window quits the process
/// (`quit_with_last_window`).
pub struct AlreadyRunningNotice {
    focus: FocusHandle,
}

impl AlreadyRunningNotice {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self { focus }
    }
}

impl Render for AlreadyRunningNotice {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .key_context(NOTICE_CONTEXT)
            .track_focus(&self.focus)
            .on_action(|_: &Dismiss, window, _| window.remove_window())
            .size_full()
            .p_5()
            .gap_2()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .text_base()
                    .strong(FontWeight::SEMIBOLD, cx)
                    .child(t!("single_instance.title")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("single_instance.body")),
            )
            .child(div().flex_1())
            .child(
                h_flex().justify_end().child(
                    Button::new("close")
                        .primary()
                        .label(t!("single_instance.close"))
                        .on_click(|_, window, _| window.remove_window()),
                ),
            )
    }
}

/// Opens the [`AlreadyRunningNotice`] window at `ui_scale` and binds its
/// keys. Call after `gpui_kit::init`, once per process.
pub fn open_notice(
    ui_scale: f32,
    cx: &mut App,
) -> gpui_kit::Result<(AnyWindowHandle, Entity<AlreadyRunningNotice>)> {
    cx.bind_keys([
        KeyBinding::new("enter", Dismiss, Some(NOTICE_CONTEXT)),
        KeyBinding::new("escape", Dismiss, Some(NOTICE_CONTEXT)),
    ]);
    // The window is sized in the theme's base font so it follows the scale;
    // before `theme::apply` that is the default theme's, which `apply`
    // keeps.
    let rem = Theme::global(cx).font_size * crate::scale::snap_scale(ui_scale);
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(t!("app.title").to_string().into()),
            ..Default::default()
        }),
        app_id: Some(crate::APP_ID.into()),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(rem * 24., rem * 9.),
            cx,
        ))),
        is_resizable: false,
        is_minimizable: false,
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, move |window, cx| {
        crate::style_window(ui_scale, window, cx);
        cx.new(|cx| AlreadyRunningNotice::new(window, cx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn single_instance_second_acquire_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path().join("data"));

        let first = acquire(&data).expect("first acquire");
        assert!(first.unguarded_reason().is_none());
        assert!(tmp.path().join("data").join(LOCK_FILE).is_file());

        // A second open file description of the same file, as a second
        // process would have, cannot take the lock.
        assert_eq!(acquire(&data).unwrap_err(), AlreadyRunning);

        // Dropping the guard releases it. A test elsewhere in this binary
        // forking a child can keep the descriptor alive until the child
        // execs, so allow a short grace period.
        drop(first);
        let deadline = Instant::now() + Duration::from_secs(2);
        let again = loop {
            match acquire(&data) {
                Ok(g) => break g,
                Err(AlreadyRunning) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(e) => panic!("lock not released: {e}"),
            }
        };
        assert!(again.unguarded_reason().is_none());
    }

    #[test]
    fn unusable_data_dir_runs_unguarded() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the data dir should be: the lock cannot be created.
        let blocker = tmp.path().join("data");
        std::fs::write(&blocker, "").unwrap();
        let guard = acquire(&DataDirs::new(&blocker)).expect("unguarded, not refused");
        assert!(guard.unguarded_reason().is_some());
    }
}
