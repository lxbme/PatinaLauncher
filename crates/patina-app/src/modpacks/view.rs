use super::model::{PackView, Pane, RequestOrigin};
use crate::{installation, layout::scroll_y};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme, Disableable, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    dialog::DialogFooter,
    h_flex,
    input::Input,
    label::Label,
    progress::Progress,
    v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, px, rems, size,
};
use patina_core::modpack::{PackChangeKind, PackConflictReason, PackDecision};
use rust_i18n::t;
use std::rc::Rc;

impl Render for PackView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Runtime geometry is clamped against the window; product dimensions remain rem-based.
        let preferred = if self.pane != Pane::Confirm {
            29.
        } else if self.running || self.loading {
            24.
        } else if self.done {
            12.
        } else if self.plan.is_some() {
            26.
        } else if self.preview.is_some() {
            20.
        } else {
            14.
        };
        let height = (f32::from(window.viewport_size().height) / f32::from(window.rem_size())
            - 12.)
            .max(10.)
            .min(preferred);
        let body = if self.pane == Pane::Confirm {
            self.confirm(window, cx)
        } else {
            self.list(window, cx)
        };
        v_flex()
            .id("modpack-surface")
            .test_support()
            .w_full()
            .h(rems(height))
            .min_h_0()
            .gap_3()
            .text_sm()
            .child(body)
    }
}
impl PackView {
    fn confirm(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mut content = v_flex()
            .id("modpack-confirm")
            .test_support()
            .gap_4()
            .min_w_0();
        if let Some((name, version)) = &self.identity {
            content = content.child(installation::identity(
                "modpack-identity",
                name.clone(),
                version.clone(),
                cx,
            ));
        }
        if let Some(error) = &self.error {
            content = content.child(installation::failure("modpack-error", error.clone()));
            if self.preview.is_some()
                && self.instance.is_some()
                && !self.fresh
                && self.retry.is_none()
                && !self.running
            {
                content = content.child(
                    Button::new("modpack-refresh-preview")
                        .outline()
                        .disabled(self.loading)
                        .label(t!("modpacks.refresh_preview").to_string())
                        .on_click(cx.listener(|v, _, _, cx| v.replan(cx))),
                );
            }
        }
        if self.done {
            content = content.child(
                div()
                    .id("modpack-complete")
                    .test_support()
                    .child(Label::new(t!("modpacks.complete").to_string()).text_sm())
                    .children((self.retained > 0).then(|| {
                        Label::new(
                            t!("modpacks.custom_complete", count = self.retained).to_string(),
                        )
                        .text_sm()
                    })),
            );
        } else if self.running || self.loading {
            let progress = self.progress.as_ref().map(crate::launch::progress::view_of);
            let step = progress.as_ref().map(|p| p.step()).unwrap_or_else(|| {
                t!(if self.running {
                    "modpacks.preparing"
                } else if matches!(self.origin, RequestOrigin::OnlineVersions(_)) {
                    "modpacks.loading_versions"
                } else {
                    "modpacks.loading"
                })
                .to_string()
            });
            let mut status = v_flex()
                .id("modpack-progress")
                .test_support()
                .aria_label(step.clone())
                .role(gpui_kit::Role::Status)
                .gap_2()
                .child(Label::new(step).text_sm())
                .child(
                    Progress::new("modpack-progress-bar")
                        .loading(progress.as_ref().is_none_or(|p| p.fraction.is_none()))
                        .value(progress.as_ref().and_then(|p| p.fraction).unwrap_or(0.) * 100.),
                );
            if let Some(amount) = progress.and_then(|p| p.amount) {
                status = status.child(
                    Label::new(amount)
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                );
            }
            content = content.child(status).children(installation::activity(
                self.progress.as_ref().and_then(|p| p.activity.as_deref()),
                |file| file.name.clone(),
                window,
                cx,
            ));
        } else if let Some(preview) = &self.preview {
            if self.instance.is_none() || self.fresh || self.retry.is_some() {
                content = content.child(
                    v_flex()
                        .gap_2()
                        .child(installation::section_label(
                            t!("modpacks.instance_name").to_string(),
                            cx,
                        ))
                        .child(Input::new(&self.name).id("modpack-name")),
                );
            }
            let loader = preview.loader().map_or_else(
                || t!("instances.vanilla").to_string(),
                |(kind, version)| format!("{} {version}", kind.display_name()),
            );
            let selected: Vec<_> = preview
                .files()
                .iter()
                .filter(|f| !f.is_optional() || self.optional.get(f.path()) == Some(&true))
                .collect();
            let bytes = selected.iter().map(|f| f.size()).sum();
            let count = selected
                .iter()
                .map(|f| f.path())
                .chain(preview.overrides().keys().map(String::as_str))
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            let mut metadata = v_flex()
                .id("modpack-metadata")
                .test_support()
                .gap_2()
                .child(installation::metadata(
                    t!("installation.game").to_string(),
                    format!("Minecraft {} · {loader}", preview.mc_version()),
                    cx,
                ))
                .child(installation::metadata(
                    t!("installation.files").to_string(),
                    installation::files(count, bytes),
                    cx,
                ));
            if let Some(plan) = self.plan.as_ref().filter(|_| !self.fresh) {
                let old = plan
                    .old_components()
                    .iter()
                    .map(|c| {
                        let name = match c.uid {
                            patina_core::instance::ComponentUid::Minecraft => {
                                "Minecraft".to_string()
                            }
                            patina_core::instance::ComponentUid::Forge => {
                                patina_core::loader::LoaderKind::Forge.display_name()
                            }
                            patina_core::instance::ComponentUid::Neoforge => {
                                patina_core::loader::LoaderKind::Neoforge.display_name()
                            }
                            patina_core::instance::ComponentUid::Fabric => {
                                patina_core::loader::LoaderKind::Fabric.display_name()
                            }
                        };
                        format!("{name} {}", c.version)
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                metadata = metadata.child(installation::metadata(
                    t!("installation.previous").to_string(),
                    format!("{} · {old}", plan.old_state().version()),
                    cx,
                ));
            }
            content = content.child(metadata).child(
                Label::new(t!("installation.game_extra").to_string())
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            );
            if let Some(plan) = self.plan.as_ref().filter(|_| !self.fresh) {
                if plan
                    .old_components()
                    .first()
                    .is_some_and(|c| c.version != preview.mc_version())
                {
                    content = content.child(
                        div().id("modpack-world-warning").test_support().child(
                            gpui_kit::component::alert::Alert::warning(
                                "modpack-world-risk",
                                t!("modpacks.world_warning").to_string(),
                            )
                            .small(),
                        ),
                    );
                }
                content = content.child(
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("modpack-normal")
                                        .outline()
                                        .label(
                                            t!(
                                                "modpacks.normal",
                                                count = plan.normal_change_count()
                                            )
                                            .to_string(),
                                        )
                                        .on_click(cx.listener(|v, _, _, cx| {
                                            v.pane = Pane::Changes;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("modpack-preserved")
                                        .outline()
                                        .label(
                                            t!(
                                                "modpacks.preserved",
                                                count = plan.preserved_paths().len()
                                            )
                                            .to_string(),
                                        )
                                        .on_click(cx.listener(|v, _, _, cx| {
                                            v.pane = Pane::Preserved;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            h_flex()
                                .track_focus(&self.conflict_focus)
                                .gap_3()
                                .child(
                                    div().flex_1().min_w_0().text_sm().child(
                                        t!(
                                            "modpacks.conflict_summary",
                                            count = plan.conflict_count(),
                                            pack = self.use_pack_count(),
                                            local = plan.conflict_count() - self.use_pack_count()
                                        )
                                        .to_string(),
                                    ),
                                )
                                .child(
                                    Button::new("modpack-conflicts")
                                        .outline()
                                        .disabled(plan.conflict_count() == 0)
                                        .label(t!("modpacks.resolve").to_string())
                                        .on_click(cx.listener(|v, _, _, cx| {
                                            v.pane = Pane::Conflicts;
                                            cx.notify();
                                        })),
                                ),
                        ),
                );
            }
            if self.instance.is_some() && self.retry.is_none() {
                let mut choice = v_flex().gap_2().child(
                    Checkbox::new("modpack-fresh")
                        .checked(self.fresh)
                        .label(t!("modpacks.fresh").to_string())
                        .on_change(cx.listener(|v, value, _, cx| {
                            if v.fresh == *value {
                                return;
                            }
                            v.fresh = *value;
                            v.decisions = Default::default();
                            if !v.fresh {
                                v.replan(cx)
                            } else {
                                v.error = None;
                                cx.notify();
                            }
                        })),
                );
                if self.fresh {
                    choice = choice.child(
                        Label::new(t!("modpacks.fresh_hint").to_string())
                            .text_sm()
                            .text_color(cx.theme().muted_foreground),
                    );
                }
                content = content.child(choice);
            }
            let optional_files: Vec<_> = preview
                .files()
                .iter()
                .enumerate()
                .filter(|(_, f)| f.is_optional())
                .map(|(ix, _)| ix)
                .collect();
            if !optional_files.is_empty() {
                let sizes = Rc::new(vec![
                    size(px(0.), rems(2.5).to_pixels(window.rem_size()));
                    optional_files.len()
                ]);
                let optional = v_virtual_list(
                    cx.entity(),
                    "modpack-optional-rows",
                    sizes,
                    move |view, range, _, cx| {
                        range
                            .map(|ix| {
                                let path = view
                                    .preview
                                    .as_ref()
                                    .expect("optional preview retained")
                                    .files()[optional_files[ix]]
                                    .path()
                                    .to_string();
                                let selected = view.optional.get(&path).copied().unwrap_or(false);
                                Checkbox::new(SharedString::from(format!(
                                    "modpack-optional:{path}"
                                )))
                                .checked(selected)
                                .label(path.clone())
                                .on_change(cx.listener(move |v, value, _, cx| {
                                    v.set_optional(path.clone(), *value, cx)
                                }))
                                .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.optional_scroll)
                .size_full();
                content = content.child(
                    v_flex()
                        .gap_2()
                        .child(installation::section_label(
                            t!("modpacks.optional").to_string(),
                            cx,
                        ))
                        .child(
                            div()
                                .id("modpack-optional-list")
                                .test_support()
                                .relative()
                                .h_24()
                                .child(optional)
                                .vertical_scrollbar(&self.optional_scroll),
                        ),
                );
            }
        } else {
            if !self.versions.is_empty() {
                content = content.child(installation::section_label(
                    t!("modpacks.choose_online_version").to_string(),
                    cx,
                ));
            }
            let mut versions = v_flex().gap_2();
            for version in &self.versions {
                let version = version.clone();
                versions = versions.child(
                    Button::new(SharedString::from(format!(
                        "modpack-version:{}",
                        version.reference().id()
                    )))
                    .outline()
                    .label(format!(
                        "{} · {}",
                        version.name(),
                        version.game_versions().join(", ")
                    ))
                    .on_click(cx.listener(move |v, _, window, cx| {
                        v.preview_online(version.clone(), window, cx)
                    })),
                );
            }
            content = content.child(versions);
            if matches!(self.origin, RequestOrigin::Local(_)) {
                content = content.child(
                    Button::new("modpack-local")
                        .outline()
                        .label(t!("modpacks.choose_local").to_string())
                        .on_click(cx.listener(|v, _, window, cx| v.choose_local(window, cx))),
                );
            }
        }
        let can_submit = self.preview.is_some()
            && !self.loading
            && !self.running
            && !self.done
            && (self.instance.is_none()
                || self.fresh
                || self.retry.is_some()
                || self.plan.is_some());
        let applying = self.running
            && self
                .progress
                .as_ref()
                .is_some_and(|p| p.phase == patina_core::progress::Phase::Finalize);
        let mut footer = DialogFooter::new()
            .gap_2()
            .flex_none()
            .pt_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("modpack-cancel")
                    .label(
                        t!(if self.done {
                            "modpacks.done"
                        } else if self.running {
                            "modpacks.cancel_download"
                        } else {
                            "modpacks.cancel"
                        })
                        .to_string(),
                    )
                    .disabled(applying)
                    .on_click(cx.listener(|v, _, window, cx| {
                        if v.dismiss(window, cx) {
                            window.close_dialog(cx)
                        }
                    })),
            );
        if !self.loading && !self.running && !self.done {
            if self.preview.is_some() {
                footer = footer.child(
                    Button::new("modpack-submit")
                        .primary()
                        .disabled(!can_submit)
                        .label(
                            t!(if self.retry.is_some() {
                                "modpacks.retry"
                            } else if self.instance.is_some() && !self.fresh {
                                "modpacks.update"
                            } else {
                                "modpacks.install"
                            })
                            .to_string(),
                        )
                        .on_click(cx.listener(|v, _, _, cx| v.submit(cx))),
                );
            } else if self.error.is_some() {
                footer = footer.child(
                    Button::new("modpack-request-retry")
                        .primary()
                        .label(t!("modpacks.retry_request").to_string())
                        .on_click(cx.listener(|v, _, window, cx| v.retry_request(window, cx))),
                );
            }
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(
                div().flex_1().min_h_0().child(scroll_y(
                    div()
                        .id("modpack-confirm-scroll")
                        .test_support()
                        .child(content),
                    &self.scroll,
                )),
            )
            .child(
                div()
                    .id("modpack-footer")
                    .test_support()
                    .flex_none()
                    .child(footer),
            )
            .into_any_element()
    }
    fn file_row(
        &self,
        change: &patina_core::modpack::PackChange,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = change.id().to_string();
        let label = change
            .target_path()
            .unwrap_or(change.current_path())
            .to_string();
        let mut row = v_flex()
            .id(SharedString::from(format!("modpack-row:{id}")))
            .gap_1()
            .py_2()
            .min_w_0()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(label),
            );
        if self.pane == Pane::Conflicts {
            let reason = match change.reason() {
                Some(PackConflictReason::DeletedLocally) => "modpacks.reason_deleted",
                Some(PackConflictReason::RemovedByPack) => "modpacks.reason_removed",
                Some(PackConflictReason::NewPathCollision) => "modpacks.reason_collision",
                _ => "modpacks.reason_both",
            };
            let use_pack = self.decisions.decision(change) == PackDecision::UsePack;
            row = row
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!(reason).to_string()),
                )
                .child(
                    Checkbox::new(SharedString::from(format!("modpack-use:{id}")))
                        .checked(use_pack)
                        .label(
                            t!(if change.is_pack_deletion() {
                                "modpacks.use_delete"
                            } else {
                                "modpacks.use_pack"
                            })
                            .to_string(),
                        )
                        .on_change(cx.listener(move |v, value, _, cx| {
                            v.decisions.set(
                                id.clone(),
                                if *value {
                                    PackDecision::UsePack
                                } else {
                                    PackDecision::KeepLocal
                                },
                            );
                            cx.notify();
                        })),
                );
        } else {
            row = row.child(
                t!(if change.is_pack_deletion() {
                    "modpacks.delete_file"
                } else {
                    "modpacks.replace_file"
                })
                .to_string(),
            );
        }
        row.into_any_element()
    }
    fn list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let indices: Vec<usize> = self
            .plan
            .as_ref()
            .map(|p| {
                p.changes()
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| {
                        if self.pane == Pane::Conflicts {
                            c.kind() == PackChangeKind::Conflict
                        } else {
                            c.kind() == PackChangeKind::FollowPack
                        }
                    })
                    .map(|(ix, _)| ix)
                    .collect()
            })
            .unwrap_or_default();
        let count = if self.pane == Pane::Preserved {
            self.plan.as_ref().map_or(0, |p| p.preserved_paths().len())
        } else {
            indices.len()
        };
        let row_height = rems(if self.pane == Pane::Conflicts {
            5.5
        } else {
            3.5
        })
        .to_pixels(window.rem_size());
        // Rebuilt with rem each frame, so zoom cannot leave stale row measurements.
        let sizes = Rc::new(vec![size(px(0.), row_height); count]);
        let rows = v_virtual_list(
            cx.entity(),
            "modpack-list-rows",
            sizes,
            move |view, range, _, cx| {
                range
                    .map(|ix| {
                        let Some(plan) = view.plan.as_ref() else {
                            return div().into_any_element();
                        };
                        if view.pane == Pane::Preserved {
                            let path = &plan.preserved_paths()[ix];
                            div()
                                .id(SharedString::from(format!("modpack-preserved:{path}")))
                                .py_2()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(path.clone())
                                .into_any_element()
                        } else {
                            view.file_row(&plan.changes()[indices[ix]], cx)
                        }
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.list_scroll)
        .flex_1()
        .min_h_0();
        let mut top = v_flex().flex_none().gap_2();
        if self.pane == Pane::Conflicts {
            top = top.child(t!("modpacks.conflict_hint").to_string()).child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("modpack-bulk-local")
                            .outline()
                            .label(t!("modpacks.all_local").to_string())
                            .on_click(cx.listener(|v, _, _, cx| {
                                if let Some(p) = &v.plan {
                                    for c in p
                                        .changes()
                                        .iter()
                                        .filter(|c| c.kind() == PackChangeKind::Conflict)
                                    {
                                        v.decisions.set(c.id(), PackDecision::KeepLocal)
                                    }
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("modpack-bulk-pack")
                            .outline()
                            .label(t!("modpacks.all_pack").to_string())
                            .on_click(cx.listener(|v, _, _, cx| {
                                if let Some(p) = &v.plan {
                                    for c in p
                                        .changes()
                                        .iter()
                                        .filter(|c| c.kind() == PackChangeKind::Conflict)
                                    {
                                        v.decisions.set(c.id(), PackDecision::UsePack)
                                    }
                                }
                                cx.notify();
                            })),
                    ),
            );
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(top)
            .child(
                div()
                    .id("modpack-list")
                    .test_support()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(rows)
                    .vertical_scrollbar(&self.list_scroll),
            )
            .child(
                h_flex().justify_end().flex_none().child(
                    Button::new("modpack-list-done")
                        .label(t!("modpacks.done").to_string())
                        .on_click(cx.listener(|v, _, window, cx| {
                            v.pane = Pane::Confirm;
                            v.conflict_focus.focus(window, cx);
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }
}
