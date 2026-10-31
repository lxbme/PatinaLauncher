//! 关于: the version, the license and where the source code lives; a
//! development build says it is one.

use gpui_kit::component::link::Link;
use gpui_kit::component::{ActiveTheme, ThemeStyled, h_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, Context, InteractiveElement, IntoElement, KeyDownEvent, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div, rems,
};
use rust_i18n::t;

use super::{Section, SettingsPage};

/// The project's home, opened in the browser.
pub const REPO_URL: &str = "https://github.com/lxbme/PatinaLauncher";
/// The license Patina is released under (an SPDX expression; not
/// translated).
pub const LICENSE: &str = "GPL-3.0-or-later";

impl SettingsPage {
    pub(super) fn render_about(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let version = t!("shell.status.version", version = env!("CARGO_PKG_VERSION"));
        let version = SharedString::from(version.to_string());
        Self::section(Section::About, cx)
            .border_b_0()
            .child(Self::about_row(
                "prefs-version",
                t!("prefs.version").to_string(),
                div().child(version).into_any_element(),
                cx,
            ))
            .child(Self::about_row(
                "prefs-license",
                t!("prefs.license").to_string(),
                div().child(LICENSE).into_any_element(),
                cx,
            ))
            .child(Self::about_row(
                "prefs-repo",
                t!("prefs.repo").to_string(),
                self.render_repo_link(window, cx),
                cx,
            ))
            .children(dev_build_note(cx))
    }

    /// The repository as a link (it opens in the browser), inside a Tab
    /// stop of its own: Enter or Space opens it too, and the focus ring
    /// shows where the keyboard is.
    fn render_repo_link(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.repo_focus.is_focused(window);
        let open = self.url_opener.clone();
        let open_by_key = self.url_opener.clone();
        div()
            .id("prefs-repo-link")
            .test_support()
            .track_focus(&self.repo_focus)
            .aria_label(REPO_URL)
            .rounded(cx.theme().radius)
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    open_by_key(REPO_URL, cx);
                    cx.stop_propagation();
                }
            })
            .when(focused, |link| link.focus_ring_style(window, cx))
            .child(
                Link::new("prefs-repo-anchor")
                    .on_click(move |_, _, cx| open(REPO_URL, cx))
                    .child(REPO_URL),
            )
            .into_any_element()
    }

    /// A label and its value on one line.
    fn about_row(
        id: &'static str,
        label: String,
        value: gpui_kit::AnyElement,
        cx: &App,
    ) -> gpui_kit::AnyElement {
        h_flex()
            .id(id)
            .test_support()
            .gap_3()
            .child(
                div()
                    .w(rems(7.))
                    .flex_none()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(
                div()
                    .min_w_0()
                    .text_color(cx.theme().foreground)
                    .child(value),
            )
            .into_any_element()
    }
}

/// "开发构建（包含离线账号）", in development builds only.
#[cfg(feature = "dev-offline")]
fn dev_build_note(cx: &App) -> Option<gpui_kit::AnyElement> {
    Some(SettingsPage::hint(
        "prefs-dev-build",
        t!("prefs.dev_build").to_string(),
        cx,
    ))
}

/// Release builds are not development builds.
#[cfg(not(feature = "dev-offline"))]
fn dev_build_note(_: &App) -> Option<gpui_kit::AnyElement> {
    None
}
