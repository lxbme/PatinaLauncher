//! Selected project metadata, compatible files and the installation entry point.
use super::{Detail, DownloadsPage};
use crate::fonts::Strong;
use crate::layout::{GUTTER, STRIP_H, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY, scroll_y};
use gpui_kit::base::Link;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::select::Select;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, v_flex};
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Styled,
    TestSupportExt as _, div, rems,
};
use patina_core::content::online::{ContentKind, Platform};
use rust_i18n::t;

impl DownloadsPage {
    pub(super) fn render_detail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (border, muted, link) = (
            cx.theme().border,
            cx.theme().muted_foreground,
            cx.theme().link,
        );
        let model = self.model.read(cx);
        let instance = model.target().and_then(|id| {
            self.instances
                .read(cx)
                .instances()
                .iter()
                .find(|i| i.id == id)
                .map(|i| i.config.name.clone())
        });
        let installable = model.install_request().is_some()
            && model
                .target()
                .is_some_and(|id| !self.instances.read(cx).is_busy(id, cx));
        let label = match &instance {
            Some(name) => t!("downloads.detail.confirm_to", instance = name).to_string(),
            None => t!("downloads.detail.choose_instance").to_string(),
        };
        let reason = if model.kind() == ContentKind::Modpack {
            "downloads.detail.modpack_later"
        } else if model.mc_version().is_none() {
            "downloads.detail.choose_compatibility"
        } else if model.kind() == ContentKind::Mod && model.loader().is_none() {
            "downloads.detail.needs_loader"
        } else if model.target().is_none() {
            "downloads.detail.choose_compatibility"
        } else {
            "downloads.detail.no_compatible_files"
        };
        let mut body = v_flex()
            .id("downloads-detail")
            .test_support()
            .gap_3()
            .p(GUTTER)
            .text_size(TEXT_PRIMARY);
        body = match model.detail() {
            Detail::Idle => body.child(
                div()
                    .id("downloads-detail-empty")
                    .test_support()
                    .text_color(muted)
                    .child(t!("downloads.detail.empty").to_string()),
            ),
            Detail::Loading => body.child(t!("downloads.detail.loading").to_string()),
            Detail::Failed(error) => body.child(error.clone()).child(
                Button::new("downloads-detail-retry")
                    .outline()
                    .label(t!("downloads.results.retry").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model.update(cx, |m, cx| m.retry_detail(cx))
                    })),
            ),
            Detail::Loaded { project, versions } => {
                let description = if project.reference().platform() == Platform::CurseForge {
                    TextView::html("downloads-description", project.description().to_string())
                } else {
                    TextView::markdown("downloads-description", project.description().to_string())
                };
                let mut body = body
                    .child(
                        div()
                            .text_size(TEXT_ROW)
                            .strong(FontWeight::MEDIUM, cx)
                            .child(project.title().to_string()),
                    )
                    .child(
                        div().child(
                            t!(
                                "downloads.results.by_author",
                                author = project.authors().join(", ")
                            )
                            .to_string(),
                        ),
                    )
                    .child(
                        div().id("downloads-project-page").test_support().child(
                            Link::new("downloads-project-link")
                                .focus_visible(move |s| s.bg(border))
                                .href(project.url().to_string())
                                .text_color(link)
                                .open_with(|url, _, _, cx| {
                                    if url.starts_with("https://") || url.starts_with("http://") {
                                        cx.open_url(url);
                                    }
                                })
                                .child(t!("downloads.detail.project_page").to_string()),
                        ),
                    );
                for url in project.support_urls() {
                    body = body.child(
                        Link::new(gpui_kit::SharedString::from(format!(
                            "downloads-support:{url}"
                        )))
                        .focus_visible(move |s| s.bg(border))
                        .href(url.clone())
                        .text_color(link)
                        .open_with(|url, _, _, cx| {
                            if url.starts_with("https://") || url.starts_with("http://") {
                                cx.open_url(url);
                            }
                        })
                        .child(t!("downloads.detail.support").to_string()),
                    );
                }
                body = body
                    .child(description)
                    .child(
                        Checkbox::new("downloads-preview-channels")
                            .checked(model.preview_channels())
                            .label(t!("downloads.detail.preview_channels").to_string())
                            .on_change(cx.listener(|this, value, _, cx| {
                                this.model
                                    .update(cx, |m, cx| m.set_preview_channels(*value, cx))
                            })),
                    )
                    .child(
                        Select::new(&self.file_version)
                            .id("downloads-file-version")
                            .small()
                            .w_full()
                            .disabled(versions.is_empty())
                            .accessibility_label(t!("downloads.detail.file_version").to_string()),
                    );
                if versions.is_empty() {
                    body = body.child(
                        div().text_color(muted).child(
                            t!(if model.mc_version().is_none() {
                                "downloads.detail.choose_compatibility"
                            } else if model.kind() == ContentKind::Mod && model.loader().is_none() {
                                "downloads.detail.needs_loader"
                            } else {
                                "downloads.detail.no_compatible_files"
                            })
                            .to_string(),
                        ),
                    );
                }
                if let Some(version) = model.selected_version() {
                    body = body.child(
                        div().child(
                            t!(
                                "downloads.detail.compatibility",
                                versions = version.game_versions().join(", ")
                            )
                            .to_string(),
                        ),
                    );
                }
                body
            }
        };
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
                    .child(t!("downloads.detail.title").to_string()),
            )
            .child(div().flex_1().min_h_0().child(scroll_y(
                div().id("downloads-detail-scroll").child(body),
                &self.detail_scroll,
            )))
            .child(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .p(GUTTER)
                    .border_t_1()
                    .border_color(border)
                    .children(
                        (!installable && !matches!(model.detail(), Detail::Loaded { .. })).then(
                            || {
                                div()
                                    .text_size(TEXT_SECONDARY)
                                    .text_color(muted)
                                    .child(t!(reason).to_string())
                            },
                        ),
                    )
                    .child(
                        Button::new("downloads-install")
                            .primary()
                            .w_full()
                            .min_w(rems(0.))
                            .disabled(!installable)
                            .label(label)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(request) = this.model.read(cx).install_request() {
                                    super::install::open_install(
                                        this.instances.clone(),
                                        request,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    ),
            )
    }
}
