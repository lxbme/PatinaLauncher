//! The filters column: platform, game version, loader, category and the
//! target instance.

use gpui_kit::component::button::Button;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::select::Select;
use gpui_kit::component::{ActiveTheme, Sizable, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Styled,
    TestSupportExt as _, Window, div,
};
use patina_core::content::online::Platform;
use rust_i18n::t;

use super::{DownloadsPage, Versions, takes_loader};
use crate::fonts::Strong;
use crate::layout::{GUTTER, STRIP_H, TEXT_PRIMARY, TEXT_SECONDARY, scroll_y};

impl DownloadsPage {
    pub(super) fn render_filters(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let model = self.model.read(cx);
        let platform = model.platform();
        let show_loader = takes_loader(model.kind());
        let failed = self.versions() == Versions::Failed;

        let platforms = RadioGroup::horizontal("downloads-platform")
            .selected_index(Some(usize::from(platform == Platform::CurseForge)))
            .child(Radio::new("modrinth").label(t!("downloads.platform.modrinth").to_string()))
            .child(Radio::new("curseforge").label(t!("downloads.platform.curseforge").to_string()))
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                let platform = if *ix == 1 {
                    Platform::CurseForge
                } else {
                    Platform::Modrinth
                };
                this.model.update(cx, |m, cx| m.set_platform(platform, cx));
            }));

        let version = Select::new(&self.version)
            .id("downloads-version")
            .small()
            .w_full()
            .accessibility_label(t!("downloads.filters.game_version").to_string())
            .search_placeholder(t!("downloads.filters.search_versions").to_string());
        let loader = Select::new(&self.loader)
            .id("downloads-loader")
            .small()
            .w_full()
            .accessibility_label(t!("downloads.filters.loader").to_string());
        // The source offers no categories yet: the one entry, disabled.
        let category = Select::new(&self.category)
            .id("downloads-category")
            .small()
            .w_full()
            .disabled(true)
            .accessibility_label(t!("downloads.filters.category").to_string());
        let target = Select::new(&self.target)
            .id("downloads-target")
            .small()
            .w_full()
            .accessibility_label(t!("downloads.filters.target").to_string());

        let field = |label: String, control: AnyElement| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_size(TEXT_SECONDARY)
                        .text_color(muted)
                        .child(label),
                )
                .child(control)
        };
        let body = v_flex()
            .id("downloads-filters")
            .gap_3()
            .p(GUTTER)
            .child(field(
                t!("downloads.filters.platform").to_string(),
                platforms.into_any_element(),
            ))
            .child(field(
                t!("downloads.filters.target").to_string(),
                target.into_any_element(),
            ))
            .child(field(
                t!("downloads.filters.game_version").to_string(),
                version.into_any_element(),
            ))
            .when(failed, |b| {
                b.child(
                    v_flex()
                        .id("downloads-versions-failed")
                        .test_support()
                        .gap_1()
                        .text_size(TEXT_SECONDARY)
                        .text_color(muted)
                        .child(t!("downloads.filters.versions_failed").to_string())
                        .child(
                            Button::new("downloads-versions-retry")
                                .outline()
                                .xsmall()
                                .label(t!("downloads.results.retry").to_string())
                                .on_click(cx.listener(|this, _, _, cx| this.ensure_versions(cx))),
                        ),
                )
            });
        let body = if show_loader {
            body.child(field(
                t!("downloads.filters.loader").to_string(),
                loader.into_any_element(),
            ))
        } else {
            body
        }
        .child(field(
            t!("downloads.filters.category").to_string(),
            category.into_any_element(),
        ));

        v_flex()
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .h(STRIP_H)
                    .px(GUTTER)
                    .border_b_1()
                    .border_color(border)
                    .text_size(TEXT_PRIMARY)
                    .strong(FontWeight::MEDIUM, cx)
                    .child(t!("downloads.filters.title").to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(scroll_y(body, &self.filters_scroll)),
            )
    }
}
