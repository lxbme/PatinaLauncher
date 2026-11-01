//! Shared user-driven update list for mods, resource packs and shaders.
use crate::core_bridge::{Core, Revision, run};
use crate::instances::InstancesModel;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{Disableable, WindowExt, h_flex, v_flex};
use gpui_kit::{
    App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Task, TestSupportExt, Window, div, rems,
};
use patina_core::content::online::*;
use rust_i18n::t;
use std::collections::HashSet;

pub struct OnlineUpdates {
    instances: Entity<InstancesModel>,
    instance: String,
    kind: ContentKind,
    platform: Platform,
    preview: bool,
    result: Option<ContentUpdatePlan>,
    selected: HashSet<VersionRef>,
    error: Option<SharedString>,
    loading: bool,
    revision: Revision,
    task: Task<()>,
}
pub fn open_updates(
    instances: Entity<InstancesModel>,
    instance: String,
    kind: ContentKind,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|_| OnlineUpdates {
        instances,
        instance,
        kind,
        platform: Platform::Modrinth,
        preview: false,
        result: None,
        selected: HashSet::new(),
        error: None,
        loading: false,
        revision: Revision::default(),
        task: Task::ready(()),
    });
    let dialog_view = view.clone();
    window.open_dialog(cx, move |dialog, window, _| {
        dialog
            .title(t!("content.updates.title").to_string())
            .w(rems(36.).to_pixels(window.rem_size()))
            .child(dialog_view.clone())
    });
    view.update(cx, |v, cx| v.check(cx));
}
impl OnlineUpdates {
    fn check(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        self.result = None;
        self.selected.clear();
        let revision = self.revision.start();
        let service = Core::global(cx).content_service.clone();
        let instance = self.instance.clone();
        let selection =
            UpdateSelection::new(self.platform, self.kind).with_channels(if self.preview {
                vec![
                    ReleaseChannel::Release,
                    ReleaseChannel::Beta,
                    ReleaseChannel::Alpha,
                ]
            } else {
                vec![ReleaseChannel::Release]
            });
        cx.notify();
        self.task = cx.spawn(async move |this, cx| {
            let Ok(work) = this.update(cx, |_, cx| {
                run(cx, async move {
                    service.check_updates(instance, selection).await
                })
            }) else {
                return;
            };
            let result = work.await;
            this.update(cx, |this, cx| {
                if !this.revision.is_current(revision) {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(Ok(result)) => this.result = Some(result),
                    Ok(Err(e)) => this.error = Some(e.to_string().into()),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        });
    }
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = &self.result else {
            return;
        };
        let requests = result
            .updates()
            .iter()
            .filter(|u| self.selected.contains(u.candidate().reference()))
            .map(|u| u.request().clone())
            .collect::<Vec<_>>();
        if requests.is_empty() {
            return;
        }
        window.close_dialog(cx);
        crate::downloads::install::open_installs(self.instances.clone(), requests, window, cx);
    }
}
impl Render for OnlineUpdates {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = v_flex()
            .id("content-update-list")
            .gap_3()
            .max_h(rems(28.))
            .overflow_y_scroll();
        if self.loading {
            body = body.child(t!("content.updates.checking").to_string());
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .id("content-updates-error")
                    .test_support()
                    .child(error.clone()),
            );
        }
        if let Some(result) = &self.result {
            if result.updates().is_empty() {
                body = body.child(
                    div()
                        .id("content-updates-current")
                        .test_support()
                        .child(t!("content.updates.current").to_string()),
                );
            }
            for update in result.updates() {
                let reference = update.candidate().reference().clone();
                let checked = self.selected.contains(&reference);
                let platform = match reference.project().platform() {
                    Platform::Modrinth => "Modrinth",
                    Platform::CurseForge => "CurseForge",
                };
                let mut row = v_flex().gap_1().child(
                    Checkbox::new(SharedString::from(format!(
                        "content-update:{}:{}",
                        reference.project().id(),
                        reference.id()
                    )))
                    .checked(checked)
                    .label(
                        t!(
                            "content.updates.version",
                            platform = platform,
                            project = update.candidate().name(),
                            installed = update.installed().version().id()
                        )
                        .to_string(),
                    )
                    .on_change(cx.listener(move |this, value, _, cx| {
                        if *value {
                            this.selected.insert(reference.clone());
                        } else {
                            this.selected.remove(&reference);
                        }
                        cx.notify();
                    })),
                );
                if update.is_modified() {
                    row = row.child(t!("content.updates.modified").to_string());
                }
                body = body.child(row);
            }
            if !result.unknown_files().is_empty() {
                body = body.child(
                    t!(
                        "content.updates.unknown",
                        count = result.unknown_files().len()
                    )
                    .to_string(),
                );
            }
        }
        let disabled = self.loading || self.selected.is_empty();
        v_flex()
            .gap_4()
            .child(
                RadioGroup::horizontal("content-update-platform")
                    .selected_index(Some(usize::from(self.platform == Platform::CurseForge)))
                    .child(
                        Radio::new("modrinth").label(t!("downloads.platform.modrinth").to_string()),
                    )
                    .child(
                        Radio::new("curseforge")
                            .label(t!("downloads.platform.curseforge").to_string()),
                    )
                    .on_change(cx.listener(|this, ix, _, cx| {
                        this.platform = if *ix == 1 {
                            Platform::CurseForge
                        } else {
                            Platform::Modrinth
                        };
                        this.check(cx);
                    })),
            )
            .child(
                Checkbox::new("content-update-preview")
                    .checked(self.preview)
                    .label(t!("downloads.detail.preview_channels").to_string())
                    .on_change(cx.listener(|this, value, _, cx| {
                        this.preview = *value;
                        this.check(cx);
                    })),
            )
            .child(body)
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("content-update-close")
                            .label(t!("downloads.install.cancel").to_string())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("content-update-retry")
                            .outline()
                            .disabled(self.loading)
                            .label(t!("downloads.results.retry").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.check(cx))),
                    )
                    .child(
                        Button::new("content-update-apply")
                            .primary()
                            .disabled(disabled)
                            .label(t!("content.updates.apply").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                    ),
            )
    }
}
