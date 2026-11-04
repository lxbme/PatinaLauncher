use crate::{
    core_bridge::{Core, Revision, forward_progress, run, run_blocking},
    instances::InstancesModel,
};
use gpui_kit::component::{WindowExt, input::InputState};
use gpui_kit::{App, AppContext, Context, Entity, ParentElement, SharedString, Task, Window, rems};
use patina_core::{
    content::online::{ContentVersion, ProjectRef},
    modpack::*,
    progress::{Phase, TaskProgress},
};
use rust_i18n::t;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Pane {
    Confirm,
    Conflicts,
    Changes,
    Preserved,
}
#[derive(Clone)]
pub(super) enum RequestOrigin {
    Local(Option<PathBuf>),
    OnlineVersion(ContentVersion),
    OnlineVersions(ProjectRef),
    Instance,
}
pub struct PackView {
    pub(super) instances: Entity<InstancesModel>,
    pub(super) instance: Option<String>,
    pub(super) preview: Option<PackPreview>,
    pub(super) origin: RequestOrigin,
    pub(super) identity: Option<(String, String)>,
    pub(super) plan: Option<PackUpdatePlan>,
    pub(super) versions: Vec<ContentVersion>,
    pub(super) name: Entity<InputState>,
    pub(super) optional: BTreeMap<String, bool>,
    pub(super) decisions: PackDecisions,
    pub(super) pane: Pane,
    pub(super) error: Option<SharedString>,
    pub(super) loading: bool,
    pub(super) running: bool,
    pub(super) done: bool,
    pub(super) fresh: bool,
    pub(super) retained: usize,
    pub(super) progress: Option<TaskProgress>,
    pub(super) scroll: gpui_kit::ScrollHandle,
    pub(super) list_scroll: gpui_kit::component::VirtualListScrollHandle,
    pub(super) optional_scroll: gpui_kit::component::VirtualListScrollHandle,
    pub(super) conflict_focus: gpui_kit::FocusHandle,
    pub(super) cancel: tokio::sync::watch::Sender<bool>,
    revision: Revision,
    work: Task<()>,
    progress_task: Task<()>,
    pub(super) retry: Option<String>,
}
fn open(
    instances: Entity<InstancesModel>,
    instance: Option<String>,
    origin: RequestOrigin,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PackView> {
    let view = cx.new(|cx| PackView::new(instances, instance, origin, window, cx));
    let owner = view.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        let running = owner.read(cx).running;
        let cancel_view = owner.clone();
        let title = if owner.read(cx).done {
            if owner.read(cx).plan.is_some()
                && !owner.read(cx).fresh
                && owner.read(cx).retry.is_none()
            {
                t!("modpacks.update_title")
            } else {
                t!("modpacks.install_title")
            }
        } else if owner.read(cx).instance.is_some()
            && !owner.read(cx).fresh
            && owner.read(cx).retry.is_none()
        {
            t!("modpacks.update_title")
        } else {
            t!("modpacks.install_title")
        }
        .to_string();
        dialog
            .title(title)
            .w(rems(38.).to_pixels(window.rem_size()))
            .max_w(
                (window.viewport_size().width - rems(2.).to_pixels(window.rem_size()))
                    .max(rems(20.).to_pixels(window.rem_size())),
            )
            .overlay_closable(false)
            .close_button(!running)
            .child(owner.clone())
            .on_ok(|_, _, cx| {
                cx.propagate();
                false
            })
            .on_cancel(move |_, window, cx| cancel_view.update(cx, |v, cx| v.dismiss(window, cx)))
    });
    view
}
pub fn open_preview(
    instances: Entity<InstancesModel>,
    preview: PackPreview,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PackView> {
    let v = open(instances, None, RequestOrigin::Local(None), window, cx);
    v.update(cx, |v, cx| v.accept_preview(preview, window, cx));
    v
}
pub fn open_update_plan(
    instances: Entity<InstancesModel>,
    plan: PackUpdatePlan,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PackView> {
    let origin = plan
        .preview()
        .source()
        .map(|v| RequestOrigin::OnlineVersions(v.project().clone()))
        .unwrap_or(RequestOrigin::Local(None));
    let v = open(
        instances,
        Some(plan.instance().id.clone()),
        origin,
        window,
        cx,
    );
    v.update(cx, |v, cx| {
        v.accept_preview(plan.preview().clone(), window, cx);
        v.optional = plan.optional_choices().clone();
        v.plan = Some(plan);
        cx.notify();
    });
    v
}
pub fn open_online(
    instances: Entity<InstancesModel>,
    version: ContentVersion,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PackView> {
    let v = open(
        instances,
        None,
        RequestOrigin::OnlineVersion(version.clone()),
        window,
        cx,
    );
    v.update(cx, |v, cx| v.preview_online(version, window, cx));
    v
}
pub fn open_local(
    instances: Entity<InstancesModel>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PackView> {
    let v = open(instances, None, RequestOrigin::Local(None), window, cx);
    v.update(cx, |v, cx| v.choose_local(window, cx));
    v
}
pub fn open_update(
    instances: Entity<InstancesModel>,
    id: String,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PackView> {
    let v = open(
        instances,
        Some(id.clone()),
        RequestOrigin::Instance,
        window,
        cx,
    );
    v.update(cx, |v, cx| v.load_update(id, cx));
    v
}
impl PackView {
    fn new(
        instances: Entity<InstancesModel>,
        instance: Option<String>,
        origin: RequestOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            instances,
            instance,
            preview: None,
            origin,
            identity: None,
            plan: None,
            versions: vec![],
            name: cx.new(|cx| InputState::new(window, cx)),
            optional: BTreeMap::new(),
            decisions: PackDecisions::default(),
            pane: Pane::Confirm,
            error: None,
            loading: false,
            running: false,
            done: false,
            fresh: false,
            retained: 0,
            progress: None,
            scroll: Default::default(),
            list_scroll: gpui_kit::component::VirtualListScrollHandle::new(),
            optional_scroll: gpui_kit::component::VirtualListScrollHandle::new(),
            conflict_focus: cx.focus_handle().tab_stop(true),
            cancel: tokio::sync::watch::channel(false).0,
            revision: Default::default(),
            work: Task::ready(()),
            progress_task: Task::ready(()),
            retry: None,
        }
    }
    pub fn use_pack_count(&self) -> usize {
        self.plan.as_ref().map_or(0, |p| {
            p.changes()
                .iter()
                .filter(|c| {
                    c.kind() == PackChangeKind::Conflict
                        && self.decisions.decision(c) == PackDecision::UsePack
                })
                .count()
        })
    }
    pub(super) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.running {
            if self
                .progress
                .as_ref()
                .is_none_or(|p| p.phase != Phase::Finalize)
            {
                tracing::info!(instance=?self.instance,phase=?self.progress.as_ref().map(|p|p.phase),"pack cancellation requested by dialog dismissal");
                let _ = self.cancel.send(true);
            }
            return false;
        }
        if self.pane != Pane::Confirm {
            self.pane = Pane::Confirm;
            self.conflict_focus.focus(window, cx);
            cx.notify();
            return false;
        }
        self.revision.start();
        let _ = self.cancel.send(true);
        true
    }
    fn accept_preview(
        &mut self,
        preview: PackPreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.name
            .update(cx, |n, cx| n.set_value(preview.name(), window, cx));
        self.identity = Some((preview.name().to_string(), preview.version().to_string()));
        self.preview = Some(preview);
        self.loading = false;
        self.error = None;
        self.done = false;
        self.pane = Pane::Confirm;
        cx.notify();
    }
    fn load_update(&mut self, id: String, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        self.preview = None;
        self.plan = None;
        self.versions.clear();
        let rev = self.revision.start();
        let store = Core::global(cx).store.clone();
        let source = Core::global(cx).content_source.clone();
        self.work = cx.spawn(async move |this, cx| {
            let Ok(job) = this.update(cx, |_, cx| {
                run_blocking(cx, move || {
                    let i = store.load(&id).map_err(PackError::from)?;
                    load_state(&i.dir)
                })
            }) else {
                return;
            };
            let state = match job.await {
                Ok(Ok(state)) => state,
                Ok(Err(e)) => {
                    let _ = this.update(cx, |v, cx| v.fail_if_current(rev, e.to_string(), cx));
                    return;
                }
                Err(e) => {
                    let _ = this.update(cx, |v, cx| v.fail_if_current(rev, e.to_string(), cx));
                    return;
                }
            };
            let Some(state) = state else {
                let _ = this.update(cx, |v, cx| {
                    if v.revision.is_current(rev) {
                        v.origin = RequestOrigin::Local(None);
                        v.loading = false;
                        v.fresh = true;
                        v.error = Some(t!("modpacks.no_baseline").to_string().into());
                        cx.notify();
                    }
                });
                return;
            };
            let reference = state.source().cloned();
            let pending = !state.is_ready();
            let _ = this.update(cx, |v, cx| {
                if !v.revision.is_current(rev) {
                    return;
                }
                v.identity = Some((state.name().to_string(), state.version().to_string()));
                v.optional = state.optional_choices().clone();
                v.retry = if pending { v.instance.clone() } else { None };
                v.origin = reference
                    .as_ref()
                    .map(|r| RequestOrigin::OnlineVersions(r.project().clone()))
                    .unwrap_or(RequestOrigin::Local(None));
                if reference.is_none() {
                    v.loading = false;
                    if pending {
                        v.error = Some(t!("modpacks.pending").to_string().into());
                    }
                    cx.notify();
                }
            });
            let Some(reference) = reference else { return };
            if !pending {
                let _ = this.update(cx, |v, cx| {
                    if v.revision.is_current(rev) {
                        v.load_versions(reference.project().clone(), cx);
                    }
                });
                return;
            }
            let expected = reference.clone();
            let Ok(job) = this.update(cx, |_, cx| {
                run(
                    cx,
                    source.version(reference.project().platform(), reference.id().to_string()),
                )
            }) else {
                return;
            };
            let result = job.await;
            let _ = this.update(cx, |v, cx| {
                if !v.revision.is_current(rev) {
                    return;
                }
                v.loading = false;
                match result {
                    Ok(Ok(version)) if version.reference() == &expected => {
                        v.origin = RequestOrigin::OnlineVersion(version);
                        v.error = Some(t!("modpacks.pending_online").to_string().into());
                    }
                    Ok(Ok(_)) => v.error = Some(PackError::SourceChanged.to_string().into()),
                    Ok(Err(e)) => v.error = Some(e.to_string().into()),
                    Err(e) => v.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        });
        cx.notify();
    }
    fn load_versions(&mut self, project: ProjectRef, cx: &mut Context<Self>) {
        self.origin = RequestOrigin::OnlineVersions(project.clone());
        self.preview = None;
        self.plan = None;
        self.versions.clear();
        self.loading = true;
        self.error = None;
        let rev = self.revision.start();
        let source = Core::global(cx).content_source.clone();
        self.work = cx.spawn(async move |this, cx| {
            let Ok(job) = this.update(cx, |_, cx| run(cx, source.pack_versions(project))) else {
                return;
            };
            let result = job.await;
            let _ = this.update(cx, |v, cx| {
                if !v.revision.is_current(rev) {
                    return;
                }
                v.loading = false;
                match result {
                    Ok(Ok(versions)) => v.versions = versions,
                    Ok(Err(e)) => v.error = Some(e.to_string().into()),
                    Err(e) => v.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        });
        cx.notify();
    }
    pub(super) fn retry_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.running {
            return;
        }
        match self.origin.clone() {
            RequestOrigin::Local(Some(path)) => self.preview_path(path, window, cx),
            RequestOrigin::Local(None) => self.choose_local(window, cx),
            RequestOrigin::OnlineVersion(version) => self.preview_online(version, window, cx),
            RequestOrigin::OnlineVersions(project) => self.load_versions(project, cx),
            RequestOrigin::Instance => {
                if let Some(id) = self.instance.clone() {
                    self.load_update(id, cx)
                }
            }
        }
    }
    fn fail_if_current(
        &mut self,
        rev: crate::core_bridge::RevisionId,
        text: String,
        cx: &mut Context<Self>,
    ) {
        if self.revision.is_current(rev) {
            self.loading = false;
            self.error = Some(text.into());
            cx.notify();
        }
    }
    pub(super) fn choose_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("modpacks.choose_local").to_string().into()),
        });
        self.work = cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                let _ = this.update_in(cx, |v, window, cx| v.preview_path(path, window, cx));
            }
        });
    }
    pub fn preview_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.origin = RequestOrigin::Local(Some(path.clone()));
        let rev = self.start_preview(cx);
        self.progress = Some(TaskProgress {
            phase: Phase::InspectPackArchive,
            done: 0,
            total: 1,
            unit: patina_core::progress::Unit::Items,

            activity: None,
        });
        self.work = cx.spawn_in(window, async move |this, cx| {
            let Ok(job) = this.update(cx, |_, cx| run_blocking(cx, move || inspect_archive(&path)))
            else {
                return;
            };
            let result = job.await;
            let _ = this.update_in(cx, |v, window, cx| {
                if !v.revision.is_current(rev) {
                    return;
                }
                match result {
                    Ok(Ok(p)) => {
                        v.accept_preview(p, window, cx);
                        v.after_preview(cx)
                    }
                    Ok(Err(e)) => v.fail_if_current(rev, e.to_string(), cx),
                    Err(e) => v.fail_if_current(rev, e.to_string(), cx),
                }
            });
        });
    }
    fn start_preview(&mut self, cx: &mut Context<Self>) -> crate::core_bridge::RevisionId {
        let rev = self.revision.start();
        let _ = self.cancel.send(true);
        self.cancel = tokio::sync::watch::channel(false).0;
        self.loading = true;
        self.error = None;
        self.done = false;
        self.preview = None;
        self.progress = None;
        self.plan = None;
        self.decisions = PackDecisions::default();
        cx.notify();
        rev
    }
    pub(super) fn preview_online(
        &mut self,
        version: ContentVersion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.identity = Some((version.name().to_string(), "Modrinth".into()));
        self.origin = RequestOrigin::OnlineVersion(version.clone());
        let rev = self.start_preview(cx);
        let cancel = self.cancel.subscribe();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let owner = cx.weak_entity();
        self.progress_task = forward_progress(cx, owner, rx, move |v, p, cx| {
            if v.revision.is_current(rev) {
                v.progress = Some(p);
                cx.notify();
            }
        });
        let service = Core::pack_service(cx);
        let source = Core::global(cx).content_source.clone();
        self.work = cx.spawn_in(window, async move |this, cx| {
            let service = match service.await {
                Ok(s) => s,
                Err(e) => {
                    let _ = this.update(cx, |v, cx| v.fail_if_current(rev, e.to_string(), cx));
                    return;
                }
            };
            let Ok(job) = this.update(cx, |_, cx| {
                run(cx, async move {
                    let project = version.reference().project().clone();
                    let preview = service.preview_online(version, cancel, Some(tx)).await?;
                    let icon = source
                        .project(project)
                        .await
                        .ok()
                        .and_then(|p| p.icon_url().map(str::to_owned));
                    Ok::<_, PackError>(preview.with_project_icon(icon))
                })
            }) else {
                return;
            };
            let result = job.await;
            let _ = this.update_in(cx, |v, window, cx| {
                if !v.revision.is_current(rev) {
                    return;
                }
                match result {
                    Ok(Ok(p)) => {
                        v.accept_preview(p, window, cx);
                        v.after_preview(cx)
                    }
                    Ok(Err(e)) => v.fail_if_current(rev, e.to_string(), cx),
                    Err(e) => v.fail_if_current(rev, e.to_string(), cx),
                }
            });
        });
    }
    fn after_preview(&mut self, cx: &mut Context<Self>) {
        if self.instance.is_some() && !self.fresh && self.retry.is_none() {
            self.replan(cx)
        } else {
            self.loading = false;
            cx.notify();
        }
    }
    pub(super) fn replan(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(preview)) = (self.instance.clone(), self.preview.clone()) else {
            return;
        };
        let optional = self.optional.clone();
        let rev = self.revision.start();
        self.plan = None;
        self.loading = true;
        self.error = None;
        let service = Core::pack_service(cx);
        self.work = cx.spawn(async move |this, cx| {
            let service = match service.await {
                Ok(s) => s,
                Err(e) => {
                    let _ = this.update(cx, |v, cx| v.fail_if_current(rev, e.to_string(), cx));
                    return;
                }
            };
            let Ok(job) = this.update(cx, |_, cx| {
                run(cx, async move {
                    service.plan_update(id, preview, optional).await
                })
            }) else {
                return;
            };
            let result = job.await;
            let _ = this.update(cx, |v, cx| {
                if !v.revision.is_current(rev) {
                    return;
                }
                v.loading = false;
                match result {
                    Ok(Ok(p)) => {
                        v.optional = p.optional_choices().clone();
                        v.plan = Some(p);
                        v.decisions = PackDecisions::default();
                        v.error = None;
                    }
                    Ok(Err(e)) => v.error = Some(e.to_string().into()),
                    Err(e) => v.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        });
        cx.notify();
    }
    pub(super) fn set_optional(&mut self, path: String, selected: bool, cx: &mut Context<Self>) {
        self.optional.insert(path, selected);
        if self.instance.is_some() && !self.fresh && self.retry.is_none() {
            self.replan(cx)
        } else {
            cx.notify();
        }
    }
    pub(super) fn submit(&mut self, cx: &mut Context<Self>) {
        if self.running || self.loading || self.done {
            return;
        }
        let Some(preview) = self.preview.clone() else {
            return;
        };
        let is_update = self.instance.is_some() && !self.fresh && self.retry.is_none();
        if is_update && self.plan.is_none() {
            return;
        }
        let name = self.name.read(cx).value().trim().to_string();
        let name = if name.is_empty() {
            preview.name().to_string()
        } else {
            name
        };
        let attempted_id = patina_core::instance::sanitize_dir_name(&name);
        let retry_store = Core::global(cx).store.clone();
        let mut choices = PackInstallChoices::new(name);
        for (p, s) in &self.optional {
            choices = choices.with_optional(p, *s)
        }
        let plan = self.plan.clone();
        let decisions = self.decisions.clone();
        let retry = self.retry.clone();
        self.running = true;
        self.error = None;
        self.progress = None;
        self.cancel = tokio::sync::watch::channel(false).0;
        let cancel = self.cancel.subscribe();
        let service = Core::pack_service(cx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let owner = cx.weak_entity();
        self.progress_task = forward_progress(cx, owner, rx, |v, p, cx| {
            v.progress = Some(p);
            cx.notify();
        });
        self.work = cx.spawn(async move |this, cx| {
            let result = match service.await {
                Ok(service) => {
                    let Ok(job) = this.update(cx, |_, cx| {
                        run(cx, async move {
                            if is_update {
                                service
                                    .apply_update(
                                        plan.ok_or(PackError::StalePlan)?,
                                        decisions,
                                        cancel,
                                        Some(tx),
                                    )
                                    .await
                            } else if let Some(id) = retry {
                                service
                                    .retry_install(id, preview, choices, cancel, Some(tx))
                                    .await
                            } else {
                                service.install(preview, choices, cancel, Some(tx)).await
                            }
                        })
                    }) else {
                        return;
                    };
                    job.await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r.map_err(|e| e.to_string()))
                }
                Err(e) => Err(e.to_string()),
            };
            let retry_id = if result.is_err() && !is_update {
                let check = this.update(cx, |_, cx| {
                    run_blocking(cx, move || {
                        retry_store
                            .load(&attempted_id)
                            .ok()
                            .filter(|i| ensure_ready(&i.dir).is_err())
                            .map(|i| i.id)
                    })
                });
                match check {
                    Ok(task) => task.await.ok().flatten(),
                    Err(_) => None,
                }
            } else {
                None
            };
            let _ = this.update(cx, |v, cx| {
                v.running = false;
                match result {
                    Ok(out) => {
                        v.done = true;
                        v.retained = out.retained_count();
                        v.instance = Some(out.instance().id.clone());
                        let model = v.instances.clone();
                        let id = out.instance().id.clone();
                        let listed = model.update(cx, |m, cx| m.refresh_and_wait(cx));
                        cx.spawn(async move |_, cx| {
                            listed.await;
                            model.update(cx, |m, cx| m.select(&id, cx));
                        })
                        .detach();
                    }
                    Err(e) => {
                        v.error = Some(e.into());
                        if !is_update {
                            v.retry = retry_id;
                        }
                        v.instances.update(cx, |m, cx| m.refresh(cx));
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestWindowExt;
    use std::io::Write;
    #[gpui_kit::test]
    fn completed_pack_without_retained_differences_has_visible_status(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let runtime = crate::runtime::start();
        cx.executor().allow_parking();
        let core = crate::core_bridge::testing::core(
            runtime.handle().clone(),
            patina_core::paths::DataDirs::new(temp.path()),
            std::sync::Arc::new(crate::core_bridge::testing::FakeCatalog::new()),
        );
        let (handle, root) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(core);
            crate::init(
                crate::settings::SettingsStore::new(temp.path().to_path_buf(), Default::default()),
                cx,
            );
            crate::open_main_window(cx).unwrap()
        });
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "modrinth.index.json",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(br#"{"formatVersion":1,"game":"minecraft","versionId":"1","name":"Completed pack","dependencies":{"minecraft":"1.20.1"},"files":[]}"#).unwrap();
        let preview =
            inspect_bytes(zip.finish().unwrap().into_inner(), ArchiveLimits::default()).unwrap();
        cx.update_window(handle, |_, window, cx| {
            let instances = root.read(cx).instances().read(cx).model().clone();
            let view = open_preview(instances, preview, window, cx);
            view.update(cx, |view, cx| {
                view.done = true;
                view.retained = 0;
                cx.notify();
            });
            window.render_frame(cx);
            assert!(
                window.find("modpack-complete").visible(),
                "successful file preparation must render a visible, nonempty completion status"
            );
        })
        .unwrap();
    }
    #[gpui_kit::test]
    fn parallel_pack_activity_has_three_row_scroll_and_fixed_footer(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let runtime = crate::runtime::start();
        cx.executor().allow_parking();
        let core = crate::core_bridge::testing::core(
            runtime.handle().clone(),
            patina_core::paths::DataDirs::new(temp.path()),
            std::sync::Arc::new(crate::core_bridge::testing::FakeCatalog::new()),
        );
        let (handle, root) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(core);
            crate::init(
                crate::settings::SettingsStore::new(temp.path().to_path_buf(), Default::default()),
                cx,
            );
            crate::open_main_window(cx).unwrap()
        });
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "modrinth.index.json",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(br#"{"formatVersion":1,"game":"minecraft","versionId":"1","name":"Completed pack","dependencies":{"minecraft":"1.20.1"},"files":[]}"#).unwrap();
        let preview =
            inspect_bytes(zip.finish().unwrap().into_inner(), ArchiveLimits::default()).unwrap();
        cx.update_window(handle, |_, window, cx| {
            let instances = root.read(cx).instances().read(cx).model().clone();
            let view = open_preview(instances, preview, window, cx);
            view.update(cx, |view, cx| {
                view.running = true;
                view.progress = Some(TaskProgress {
                    phase: Phase::DownloadContent,
                    done: 0,
                    total: 6,
                    unit: patina_core::progress::Unit::Items,
                    activity: Some(std::sync::Arc::new(
                        patina_core::download::TransferSnapshot::new(
                            7,
                            (0..6)
                                .map(|ix| {
                                    patina_core::download::FileActivity::new(
                                        format!("file-{ix}"),
                                        format!("Long file {ix}.jar"),
                                        patina_core::download::FileActivityState::Downloading,
                                    )
                                    .with_bytes(2, Some(10))
                                })
                                .collect(),
                        ),
                    )),
                });
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.find("install-file-activity").visible());
            let list = window.find("install-file-list").bounds();
            assert!(list.size.height <= rems(9.).to_pixels(window.rem_size()));
            assert!(
                window.find("modpack-cancel").visible(),
                "footer remains outside list scrolling"
            );
            window.scroll(
                "install-file-list",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
                    gpui_kit::px(0.),
                    gpui_kit::px(-600.),
                )),
                cx,
            );
            window.render_frame(cx);
            assert!(window.find("install-file:7:file-5").visible());
            assert!(window.find("modpack-cancel").visible());
            view.update(cx, |view, cx| {
                view.progress = None;
                cx.notify();
            });
            window.render_frame(cx);
            assert!(
                window.try_find("install-file-activity").is_none(),
                "empty activity must not reserve space"
            );
        })
        .unwrap();
    }
}
