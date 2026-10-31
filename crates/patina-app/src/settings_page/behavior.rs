//! 游戏启动后 (what the window does once a game has started, saved to
//! `gui.toml`) and 数据目录 (where Patina keeps its data; shown and opened,
//! not changed, in this version).

use gpui_kit::component::button::Button;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{ActiveTheme, Sizable, h_flex};
use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, div,
};
use rust_i18n::t;

use super::{Section, SettingsPage};
use crate::content::open_folder_with;
use crate::core_bridge::Core;
use crate::settings::{AfterLaunch, SettingsStore};

/// The choices in the order listed.
pub const AFTER_LAUNCH: [AfterLaunch; 3] =
    [AfterLaunch::Keep, AfterLaunch::Minimize, AfterLaunch::Close];

fn after_launch_label(choice: AfterLaunch) -> String {
    match choice {
        AfterLaunch::Keep => t!("prefs.after_keep"),
        AfterLaunch::Minimize => t!("prefs.after_minimize"),
        AfterLaunch::Close => t!("prefs.after_close"),
    }
    .to_string()
}

impl SettingsPage {
    pub(super) fn render_after_launch(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let now = SettingsStore::get(cx).after_launch;
        let radios: Vec<Radio> = AFTER_LAUNCH
            .iter()
            .map(|choice| Radio::new("after").label(after_launch_label(*choice)))
            .collect();
        Self::section(Section::AfterLaunch, cx)
            .child(
                RadioGroup::new("prefs-after-launch-choice")
                    .selected_index(AFTER_LAUNCH.iter().position(|c| *c == now))
                    .children(radios)
                    .on_click(cx.listener(|_, ix: &usize, _, cx| {
                        if let Some(choice) = AFTER_LAUNCH.get(*ix) {
                            SettingsStore::update(cx, |s| s.after_launch = *choice);
                            cx.notify();
                        }
                    })),
            )
            .child(Self::hint(
                "prefs-after-launch-hint",
                t!("prefs.after_close_hint").to_string(),
                cx,
            ))
    }

    pub(super) fn render_data(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let root = Core::global(cx).data.root();
        let path = SharedString::from(root.display().to_string());
        Self::section(Section::Data, cx)
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .id("prefs-data-path")
                            .test_support()
                            .aria_label(path.clone())
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(cx.theme().foreground)
                            .child(path),
                    )
                    .child(
                        Button::new("prefs-data-open")
                            .outline()
                            .small()
                            .label(t!("prefs.data_open").to_string())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let opener = this.opener.clone();
                                open_folder_with(root.clone(), cx, move |dir, cx| opener(dir, cx))
                                    .detach();
                            })),
                    ),
            )
            .child(Self::hint(
                "prefs-data-hint",
                t!("prefs.data_hint").to_string(),
                cx,
            ))
    }
}
