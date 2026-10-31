//! 下载: the preferred download source (a preference: the other source is
//! always the fallback) and "立即测速", which shows each source's latency
//! and, with the setting on automatic, stores the fastest as the pick.

use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    Context, ElementId, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, rems,
};
use patina_core::config::SourceChoice;
use rust_i18n::t;

use super::{Field, Section, SettingsPage};
use crate::core_bridge::{ConfigSaveError, Core, latency_text, mirror_name};

/// The source choices in the order listed.
pub const SOURCES: [SourceChoice; 3] = [
    SourceChoice::Auto,
    SourceChoice::Bmclapi,
    SourceChoice::Official,
];

/// Element id of the speed-test row of mirror `id`.
pub fn speed_row_id(id: &str) -> ElementId {
    ElementId::Name(format!("prefs-speed-{id}").into())
}

impl SettingsPage {
    fn set_source(&mut self, choice: SourceChoice, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.as_ref().map(|c| c.download.source) == Some(choice) {
            return;
        }
        self.edit(Field::Source, window, cx, move |c| {
            c.download.source = choice
        });
    }

    /// "立即测速": probes the sources; see [`Core::speed_test_now`].
    pub fn speed_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.speed.running {
            return;
        }
        self.speed.running = true;
        self.speed.error = None;
        let test = Core::speed_test_now(cx);
        self.speed.task = Some(cx.spawn_in(window, async move |this, cx| {
            let tested = test.await;
            this.update_in(cx, |this, window, cx| {
                this.speed.running = false;
                match tested {
                    Ok(tested) => {
                        this.speed.result = Some(tested.result);
                        match tested.saved {
                            Some(Ok(saved)) => Self::saved(&saved, window, cx),
                            Some(Err(error)) => {
                                let text = match &*error {
                                    ConfigSaveError::Backup(_) => error.to_string(),
                                    _ => t!("prefs.pick_save_failed", error = error).to_string(),
                                };
                                window.push_notification(Notification::error(text), cx);
                            }
                            None => {}
                        }
                    }
                    Err(error) => {
                        this.speed.error =
                            Some(t!("prefs.speed_failed", error = error).to_string().into());
                    }
                }
                this.reload_config(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn render_download(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let source = self.config.as_ref().map(|c| c.download.source);
        let radios: Vec<Radio> = SOURCES
            .iter()
            .map(|choice| Radio::new("source").label(choice.display_name()))
            .collect();
        Self::section(Section::Download, cx)
            .child(
                Self::field(t!("prefs.source").to_string(), cx)
                    .child(
                        RadioGroup::new("prefs-source")
                            .selected_index(
                                source.and_then(|s| SOURCES.iter().position(|c| *c == s)),
                            )
                            .disabled(self.config.is_none())
                            .children(radios)
                            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                                if let Some(choice) = SOURCES.get(*ix) {
                                    this.set_source(*choice, window, cx);
                                }
                            })),
                    )
                    .child(Self::hint(
                        "prefs-source-hint",
                        t!("prefs.source_hint").to_string(),
                        cx,
                    ))
                    .children(Self::error(
                        "prefs-source-error",
                        self.errors.source.as_ref(),
                        cx,
                    )),
            )
            .child(self.render_speed(cx))
    }

    fn render_speed(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let auto_pick = (self.config.as_ref())
            .filter(|c| c.download.source == SourceChoice::Auto)
            .and_then(|c| c.download.auto_pick.clone());
        let rows = (self.speed.result.iter())
            .flat_map(|r| r.latencies.iter())
            .map(|(id, latency)| {
                let (name, value) = (mirror_name(id), latency_text(*latency));
                let label = SharedString::from(format!("{name} {value}"));
                h_flex()
                    .id(speed_row_id(id))
                    .test_support()
                    .aria_label(label)
                    .gap_3()
                    .child(div().w(rems(7.)).text_color(fg).child(name))
                    .child(div().text_color(muted).child(value))
            })
            .collect::<Vec<_>>();
        let tested_at = self.speed.result.as_ref().map(|r| {
            let time = r.at.format("%H:%M:%S").to_string();
            Self::hint("prefs-speed-at", t!("prefs.tested_at", time = time), cx)
        });
        v_flex()
            .gap_2()
            .child(
                h_flex().child(
                    Button::new("prefs-speed-test")
                        .outline()
                        .small()
                        .label(t!("prefs.speed_test").to_string())
                        .loading(self.speed.running)
                        .disabled(self.speed.running)
                        .on_click(cx.listener(|this, _, window, cx| this.speed_test(window, cx))),
                ),
            )
            .child(v_flex().id("prefs-speed").gap_1().children(rows))
            .children(tested_at)
            .children(auto_pick.map(|pick| {
                Self::hint(
                    "prefs-auto-pick",
                    t!("prefs.auto_pick", mirror = mirror_name(&pick)),
                    cx,
                )
            }))
            .children(Self::error(
                "prefs-speed-error",
                self.speed.error.as_ref(),
                cx,
            ))
    }
}
