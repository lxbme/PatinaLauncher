//! Shared install confirmation/waiting surface; operations use the existing instance activity.
use crate::core_bridge::{Core, run};
use crate::installation;
use crate::instances::InstancesModel;
use crate::launch::{OpError, OpKind, OpState};
use crate::theme::ProductColors;
use futures::FutureExt;
use gpui_kit::base::Link;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{ActiveTheme, Disableable, WindowExt, v_flex};
use gpui_kit::component::{dialog::DialogFooter, label::Label, progress::Progress};
use gpui_kit::{
    App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Window,
    div, rems,
};
use patina_core::content::online::*;
use rust_i18n::t;
use std::path::PathBuf;

pub struct ContentInstallView {
    instances: Entity<InstancesModel>,
    instance: String,
    plan: Option<ContentInstallPlan>,
    pending: std::collections::VecDeque<InstallRequest>,
    request: Option<InstallRequest>,
    choices: InstallChoices,
    error: Option<SharedString>,
    running: bool,
    validating: bool,
    done: bool,
    root_selected: bool,
    current: usize,
    total: usize,
    cancel: tokio::sync::watch::Sender<bool>,
    _plan: Task<()>,
    _manual: Task<()>,
    _activity: Subscription,
}
/// Opens a read-only plan confirmation. Only its inner Install command starts application.
pub fn open_install(
    instances: Entity<InstancesModel>,
    request: InstallRequest,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContentInstallView> {
    open_installs(instances, vec![request], window, cx)
}
/// A selection of updates shares one surface; each project retains its own atomic install plan.
pub fn open_installs(
    instances: Entity<InstancesModel>,
    requests: Vec<InstallRequest>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContentInstallView> {
    let view = cx.new(|cx| ContentInstallView::new(instances, cx));
    let cancel = view.read(cx).cancel.clone();
    let dialog_view = view.clone();
    window.open_dialog(cx, move |dialog, window, _| {
        let cancel = cancel.clone();
        dialog
            .title(t!("downloads.install.title").to_string())
            .w(rems(36.).to_pixels(window.rem_size()))
            .child(dialog_view.clone())
            .on_ok(|_, _, cx| {
                cx.propagate();
                false
            })
            .on_close(move |_, _, _| {
                let _ = cancel.send(true);
            })
    });
    view.update(cx, |view, cx| {
        view.total = requests.len();
        view.pending = requests.into();
        if let Some(request) = view.pending.pop_front() {
            view.current = 1;
            view.plan(request, cx);
        }
    });
    view
}
impl ContentInstallView {
    fn new(instances: Entity<InstancesModel>, cx: &mut Context<Self>) -> Self {
        let activity = instances.read(cx).activity().clone();
        let subscription = cx.observe(&activity, |this, _, cx| {
            if this.running {
                if let Some(OpState::Failed { message, .. }) =
                    this.instances.read(cx).op(&this.instance, cx)
                {
                    this.error = Some(message.clone().into());
                    this.running = false;
                }
                cx.notify();
            }
        });
        Self {
            instances,
            instance: String::new(),
            plan: None,
            pending: Default::default(),
            request: None,
            choices: InstallChoices::default(),
            error: None,
            running: false,
            validating: false,
            done: false,
            root_selected: true,
            current: 0,
            total: 0,
            cancel: tokio::sync::watch::channel(false).0,
            _plan: Task::ready(()),
            _manual: Task::ready(()),
            _activity: subscription,
        }
    }
    fn plan(&mut self, request: InstallRequest, cx: &mut Context<Self>) {
        self.request = Some(request.clone());
        self.root_selected = true;
        self.plan = None;
        self.choices = InstallChoices::default();
        self.done = false;
        self.error = None;
        let service = Core::global(cx).content_service.clone();
        cx.notify();
        self._plan = cx.spawn(async move |this, cx| {
            let Ok(work) = this.update(cx, |_, cx| {
                run(cx, async move { service.plan_install(request).await })
            }) else {
                return;
            };
            let result = work.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(plan)) => {
                        this.instance = plan.instance_id().to_string();
                        this.plan = Some(plan);
                    }
                    Ok(Err(e)) => this.error = Some(e.to_string().into()),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        });
    }
    fn retry_plan(&mut self, cx: &mut Context<Self>) {
        if self.running || self.validating {
            return;
        }
        if let Some(request) = self.request.clone() {
            self.plan(request, cx);
        }
    }
    fn is_ready(&self) -> bool {
        self.root_selected
            && !self.running
            && !self.validating
            && !self.done
            && self.plan.as_ref().is_some_and(|plan| {
                plan.restricted_files()
                    .iter()
                    .filter(|i| i.is_selected(&self.choices))
                    .all(|i| self.choices.manual_file(i.version().reference()).is_some())
                    && plan
                        .conflicts()
                        .iter()
                        .filter(|c| {
                            plan.items().iter().any(|i| {
                                i.version().reference().project() == c.project()
                                    && i.is_selected(&self.choices)
                            })
                        })
                        .all(|c| {
                            if *c.reason() == ConflictReason::DisabledRequired {
                                self.choices.is_dependency_enabled(c.project())
                            } else {
                                self.choices.is_replacement_allowed(c.path())
                            }
                        })
            })
    }
    fn apply(&mut self, cx: &mut Context<Self>) {
        if !self.is_ready() {
            return;
        }
        let Some(plan) = self.plan.clone() else {
            return;
        };
        let choices = self.choices.clone();
        let service = Core::global(cx).content_service.clone();
        let receiver = self.cancel.subscribe();
        let done = cx.weak_entity();
        let instance = self.instance.clone();
        if *receiver.borrow() {
            return;
        }
        self.error = None;
        self.running = true;
        let started = self.instances.update(cx, |instances, cx| {
            instances.start_op(
                &self.instance,
                OpKind::InstallContent,
                move |progress| {
                    async move {
                        service
                            .apply(plan, choices, receiver, progress)
                            .await
                            .map_err(|e| OpError::Message(e.to_string()))
                    }
                    .boxed()
                },
                move |_, _, cx| {
                    crate::content::ModsEvents::emit(&instance, cx);
                    done.update(cx, |this, cx| {
                        this.running = false;
                        if let Some(request) = this.pending.pop_front() {
                            this.current += 1;
                            this.plan(request, cx);
                        } else {
                            this.done = true;
                        }
                        cx.notify();
                    })
                    .ok();
                },
                None,
                cx,
            )
        });
        if !started {
            self.running = false;
            self.error = Some(
                self.instances
                    .read(cx)
                    .busy_reason(&self.instance, cx)
                    .unwrap_or_else(|| t!("downloads.install.busy").to_string())
                    .into(),
            );
        }
        cx.notify();
    }
    fn choose_manual(&mut self, version: VersionRef, window: &mut Window, cx: &mut Context<Self>) {
        let selected = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        self._manual = cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = selected.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                this.update_in(cx, |this, window, cx| {
                    this.validate_manual(version, path, window, cx)
                })
                .ok();
            }
        });
    }
    fn validate_manual(
        &mut self,
        version: VersionRef,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self
            .plan
            .as_ref()
            .and_then(|p| {
                p.items()
                    .iter()
                    .find(|i| i.version().reference() == &version)
            })
            .map(|i| i.file().clone())
        else {
            return;
        };
        self.validating = true;
        self.error = None;
        cx.notify();
        self._manual = cx.spawn_in(window, async move |this, cx| {
            let copy = path.clone();
            let Ok(work) = this.update(cx, |_, cx| {
                run(cx, ContentService::validate_manual_file(file, copy))
            }) else {
                return;
            };
            let result = work.await;
            this.update(cx, |this, cx| {
                this.validating = false;
                match result {
                    Ok(Ok(())) => {
                        this.choices = this.choices.clone().with_manual_file(version, path)
                    }
                    Ok(Err(e)) => this.error = Some(e.to_string().into()),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        });
    }
}
struct DependencyRow<'a> {
    item: &'a PlannedContent,
    depth: usize,
    optional: bool,
    parent: Option<ProjectRef>,
}
fn dependency_rows(plan: &ContentInstallPlan) -> Vec<DependencyRow<'_>> {
    fn visit<'a>(
        plan: &'a ContentInstallPlan,
        item: &'a PlannedContent,
        depth: usize,
        optional: bool,
        parent: Option<ProjectRef>,
        seen: &mut std::collections::HashSet<ProjectRef>,
        rows: &mut Vec<DependencyRow<'a>>,
    ) {
        if !seen.insert(item.version().reference().project().clone()) {
            return;
        }
        rows.push(DependencyRow {
            item,
            depth,
            optional,
            parent,
        });
        for dependency in item.version().dependencies() {
            if !matches!(
                dependency.relation(),
                DependencyRelation::Required | DependencyRelation::Optional
            ) {
                continue;
            }
            let child = plan.items().iter().find(|child| {
                dependency
                    .version_id()
                    .is_some_and(|id| child.version().reference().id() == id)
                    || dependency
                        .project()
                        .is_some_and(|project| child.version().reference().project() == project)
            });
            if let Some(child) = child {
                visit(
                    plan,
                    child,
                    depth + 1,
                    dependency.relation() == DependencyRelation::Optional && child.is_optional(),
                    Some(item.version().reference().project().clone()),
                    seen,
                    rows,
                );
            }
        }
    }
    let mut rows = vec![];
    let mut seen = std::collections::HashSet::new();
    if let Some(root) = plan
        .items()
        .iter()
        .find(|item| item.version().reference() == plan.root())
    {
        visit(plan, root, 0, false, None, &mut seen, &mut rows);
    }
    rows
}

impl Render for ContentInstallView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let (focus_background, focus_foreground) = ProductColors::get(cx).link_focus();
        let max_body = (f32::from(window.viewport_size().height) / f32::from(window.rem_size())
            - 12.)
            .clamp(10., 26.);
        let mut body = v_flex()
            .id("content-install-body")
            .test_support()
            .gap_4()
            .min_w_0()
            .max_h(rems(max_body))
            .overflow_y_scroll();
        if let Some(plan) = &self.plan {
            if let Some(root) = plan
                .items()
                .iter()
                .find(|item| item.version().reference() == plan.root())
            {
                body = body.child(installation::identity(
                    "content-install-identity",
                    root.title().to_string(),
                    root.version().name().to_string(),
                    cx,
                ));
            }
            let target = self
                .instances
                .read(cx)
                .instances()
                .iter()
                .find(|instance| instance.id == self.instance);
            let name = target.map_or_else(
                || self.instance.clone(),
                |instance| instance.config.name.clone(),
            );
            let mut metadata = v_flex()
                .id("content-install-metadata")
                .test_support()
                .gap_2()
                .child(installation::metadata(
                    t!("installation.target").to_string(),
                    name,
                    cx,
                ));
            if let Some(target) = target {
                let loader = target.config.loader().map_or_else(
                    || t!("instances.vanilla").to_string(),
                    |(kind, component)| format!("{} {}", kind.display_name(), component.version),
                );
                metadata = metadata.child(installation::metadata(
                    t!("installation.game").to_string(),
                    format!("Minecraft {} · {loader}", target.config.mc_version()),
                    cx,
                ));
            }
            let selected: Vec<_> = plan
                .items()
                .iter()
                .filter(|item| self.root_selected && item.is_selected(&self.choices))
                .collect();
            metadata = metadata.child(installation::metadata(
                t!("installation.files").to_string(),
                installation::files(
                    selected.len(),
                    selected.iter().map(|item| item.file().size()).sum(),
                ),
                cx,
            ));
            body = body.child(metadata);
            if self.total > 1 {
                body = body.child(
                    Label::new(
                        t!(
                            "downloads.install.batch_item",
                            current = self.current,
                            total = self.total
                        )
                        .to_string(),
                    )
                    .text_sm()
                    .text_color(muted),
                );
            }
        }
        if let Some(error) = &self.error {
            body = body.child(installation::failure(
                "content-install-error",
                error.clone(),
            ));
        }
        if self.running {
            let progress = self
                .instances
                .read(cx)
                .op(&self.instance, cx)
                .and_then(|op| match op {
                    OpState::Running { progress, .. } => progress.as_ref(),
                    _ => None,
                });
            let mut status = v_flex()
                .gap_2()
                .child(
                    Label::new(progress.map_or_else(
                        || t!("downloads.install.installing").to_string(),
                        |p| p.step(),
                    ))
                    .text_sm(),
                )
                .child(
                    Progress::new("content-install-progress")
                        .loading(progress.and_then(|p| p.fraction).is_none())
                        .value(progress.and_then(|p| p.fraction).unwrap_or(0.) * 100.),
                );
            if let Some(amount) = progress.and_then(|p| p.amount.clone()) {
                status = status.child(Label::new(amount).text_sm().text_color(muted));
            }
            body = body.child(status).children(installation::activity(
                progress.and_then(|p| p.activity.as_deref()),
                |file| {
                    self.plan
                        .as_ref()
                        .and_then(|plan| {
                            plan.items().iter().find_map(|item| {
                                item.version()
                                    .files()
                                    .iter()
                                    .any(|f| f.name() == file.name)
                                    .then(|| format!("{} · {}", item.title(), file.name))
                            })
                        })
                        .unwrap_or_else(|| file.name.clone())
                },
                window,
                cx,
            ));
        } else if self.done {
            body = body.child(
                div()
                    .id("content-install-done")
                    .test_support()
                    .child(Label::new(t!("downloads.install.done").to_string()).text_sm()),
            );
        } else if let Some(plan) = &self.plan {
            let mut files = v_flex().gap_1();
            for row in dependency_rows(plan) {
                let item = row.item;
                let reference = item.version().reference();
                let root = reference == plan.root();
                let checked = self.root_selected && item.is_selected(&self.choices);
                let project = reference.project().clone();
                let parent_selected = self.root_selected
                    && row.parent.as_ref().is_none_or(|parent| {
                        plan.items()
                            .iter()
                            .find(|i| i.version().reference().project() == parent)
                            .is_some_and(|i| i.is_selected(&self.choices))
                    });
                let id = SharedString::from(if root {
                    format!("content-root:{}", reference.id())
                } else if row.optional {
                    format!("content-optional:{}", project.id())
                } else {
                    format!("content-required:{}", reference.id())
                });
                let title = if root {
                    item.file().name().to_string()
                } else {
                    format!("{} · {}", item.title(), item.version().name())
                };
                let label = if root || row.optional {
                    title
                } else {
                    t!("downloads.install.required", project = title).to_string()
                };
                let locked = self.validating || (!root && (!row.optional || !parent_selected));
                files = files.child(
                    div().py_1().pl(rems(row.depth as f32)).child(
                        Checkbox::new(id)
                            .checked(checked)
                            .disabled(locked)
                            .label(label)
                            .on_change(cx.listener(move |view, value, _, cx| {
                                if root {
                                    view.root_selected = *value
                                } else {
                                    view.choices.set_optional(project.clone(), *value)
                                }
                                cx.notify();
                            })),
                    ),
                );
            }
            body = body.child(
                v_flex()
                    .gap_2()
                    .child(installation::section_label(
                        t!("installation.selection").to_string(),
                        cx,
                    ))
                    .child(files),
            );
            let mut conflicts = v_flex().gap_2();
            let mut has_conflicts = false;
            for conflict in plan.conflicts().iter().filter(|c| {
                self.root_selected
                    && plan.items().iter().any(|i| {
                        i.version().reference().project() == c.project()
                            && i.is_selected(&self.choices)
                    })
            }) {
                has_conflicts = true;
                let conflict = conflict.clone();
                let enable = *conflict.reason() == ConflictReason::DisabledRequired;
                let checked = if enable {
                    self.choices.is_dependency_enabled(conflict.project())
                } else {
                    self.choices.is_replacement_allowed(conflict.path())
                };
                conflicts = conflicts.child(
                    Checkbox::new(SharedString::from(format!(
                        "content-conflict:{}",
                        conflict.path()
                    )))
                    .checked(checked)
                    .disabled(self.validating)
                    .label(
                        t!(
                            if enable {
                                "downloads.install.enable_dependency"
                            } else {
                                "downloads.install.replace_file"
                            },
                            file = conflict.path()
                        )
                        .to_string(),
                    )
                    .on_change(cx.listener(move |view, value, _, cx| {
                        if enable {
                            view.choices
                                .set_enabled_dependency(conflict.project().clone(), *value)
                        } else {
                            view.choices
                                .set_replacement(conflict.path().to_string(), *value)
                        }
                        cx.notify();
                    })),
                );
            }
            if has_conflicts {
                body = body.child(
                    v_flex()
                        .gap_2()
                        .child(installation::section_label(
                            t!("installation.permissions").to_string(),
                            cx,
                        ))
                        .child(conflicts),
                );
            }
            let restricted: Vec<_> = plan
                .restricted_files()
                .into_iter()
                .filter(|item| self.root_selected && item.is_selected(&self.choices))
                .collect();
            if !restricted.is_empty() {
                let mut manual = v_flex().gap_3().child(installation::section_label(
                    t!("installation.required_download").to_string(),
                    cx,
                ));
                for item in restricted {
                    let version = item.version().reference().clone();
                    let selected = self.choices.manual_file(&version).is_some();
                    let page = item.file().page_url().to_string();
                    manual = manual.child(
                        v_flex()
                            .gap_2()
                            .child(Label::new(item.file().name().to_string()).text_sm())
                            .child(
                                Link::new(SharedString::from(format!(
                                    "content-official-page:{}",
                                    version.id()
                                )))
                                .href(page)
                                .text_color(cx.theme().link)
                                .focus_visible(move |s| {
                                    s.bg(focus_background).text_color(focus_foreground)
                                })
                                .open_with(|url, _, _, cx| {
                                    if url.starts_with("https://") || url.starts_with("http://") {
                                        cx.open_url(url);
                                    }
                                })
                                .child(t!("downloads.detail.project_page").to_string()),
                            )
                            .child(
                                Button::new(SharedString::from(format!(
                                    "content-manual-file:{}",
                                    version.id()
                                )))
                                .outline()
                                .disabled(self.validating)
                                .label(
                                    t!(if selected {
                                        "downloads.install.file_verified"
                                    } else {
                                        "downloads.install.choose_file"
                                    })
                                    .to_string(),
                                )
                                .on_click(cx.listener(
                                    move |view, _, window, cx| {
                                        view.choose_manual(version.clone(), window, cx)
                                    },
                                )),
                            ),
                    );
                }
                body = body.child(manual);
            }
        } else if self.error.is_none() {
            body = body.child(
                v_flex()
                    .gap_2()
                    .child(Label::new(t!("downloads.install.planning").to_string()).text_sm())
                    .child(Progress::new("content-install-planning").loading(true)),
            );
        }
        let ready = self.is_ready();
        let mut footer = DialogFooter::new()
            .gap_2()
            .flex_none()
            .pt_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("content-install-cancel")
                    .label(
                        t!(if self.done {
                            "downloads.install.close"
                        } else {
                            "downloads.install.cancel"
                        })
                        .to_string(),
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        let _ = view.cancel.send(true);
                        window.close_dialog(cx);
                    })),
            );
        if !self.running && !self.done {
            if self.plan.is_some() {
                footer = footer.child(
                    Button::new("content-install-continue")
                        .primary()
                        .disabled(!ready)
                        .label(
                            t!(if self.error.is_some() {
                                "downloads.install.retry"
                            } else {
                                "downloads.install.continue"
                            })
                            .to_string(),
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.apply(cx))),
                );
            } else if self.error.is_some() {
                footer = footer.child(
                    Button::new("content-install-retry-plan")
                        .primary()
                        .label(t!("downloads.install.retry").to_string())
                        .on_click(cx.listener(|view, _, _, cx| view.retry_plan(cx))),
                );
            }
        }
        v_flex().text_sm().gap_4().min_w_0().child(body).child(
            div()
                .id("content-install-footer")
                .test_support()
                .flex_none()
                .child(footer),
        )
    }
}
