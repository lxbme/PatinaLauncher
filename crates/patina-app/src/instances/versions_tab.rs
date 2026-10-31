//! The "版本" tab: a Prism-style component list (the Minecraft row, the
//! loader row, and "＋ 安装加载器…" while there is no loader) with the
//! selected row's actions on the right.
//!
//! - Minecraft row: "更改游戏版本…" picks a version and runs
//!   `set_mc_version`. A loader that does not support it answers
//!   `Incompatible`; the tab then asks what to do with the loader
//!   ("换成推荐版" only when some version of it supports the new game
//!   version, "移除加载器", "取消") and runs the change again with that
//!   action.
//! - Loader row: "更换版本…" (same loader), "更换为其他加载器…" (confirms
//!   first, then installs with `replace_existing`), "修复" (reinstall) and
//!   "移除加载器…" (confirms first).
//!
//! Every change is an operation of the model ([`InstancesModel::start_op`])
//! on the instance's header, so a failure shows there with "重试". The
//! changes go through `Core.version_ops` ([`VersionOps`]). They are
//! disabled, with the reason beside them, while the instance's game runs
//! or an operation is in flight.

use std::rc::Rc;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyView, App, AppContext, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, Window, div, rems,
};
use patina_core::instance::InstanceConfig;
use patina_core::loader::ops::{LoaderAction, OpOutcome};
use patina_core::loader::{LoaderError, LoaderKind, LoaderVersion, VersionPick};
use patina_core::progress::ProgressSink;
use rust_i18n::t;

use super::loader_color;
use super::model::{Entry, InstancesModel};
use super::overview;
use super::version_picker::{LoaderPicker, VersionPicker};
use crate::core_bridge::{Core, Revision, VersionOps, run, run_blocking};
use crate::fonts::Strong;
use crate::launch::{OpError, OpKind, RetryAction};
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_SECONDARY};

/// Width of the dialogs hosting a picker at UI scale 1.0 (400 px).
const DIALOG_W: f32 = 25.;
/// Width of the actions column.
const ACTIONS_W: f32 = 16.;

/// A row of the component list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Minecraft,
    Loader,
}

/// Which versions of the installed loader are recommended and latest for
/// the instance's Minecraft version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LoaderTags {
    recommended: Option<String>,
    latest: Option<String>,
}

impl LoaderTags {
    fn of(list: &[LoaderVersion]) -> Self {
        let find =
            |f: fn(&LoaderVersion) -> bool| list.iter().find(|v| f(v)).map(|v| v.version.clone());
        Self {
            recommended: find(|v| v.recommended),
            latest: find(|v| v.latest),
        }
    }
}

/// The versions tab of one instance (an entity).
pub struct VersionsTab {
    model: Entity<InstancesModel>,
    id: String,
    row: Row,
    /// The Java the instance's Minecraft version needs, once read.
    java: Option<u32>,
    java_mc: Option<String>,
    java_revision: Revision,
    java_task: Task<()>,
    /// The loader's tags, once its versions are loaded; `tags_key` is what
    /// they are (being) loaded for.
    tags: Option<LoaderTags>,
    tags_key: Option<(LoaderKind, String)>,
    tags_revision: Revision,
    tags_task: Task<()>,
    _observe: Subscription,
}

impl VersionsTab {
    pub fn new(model: Entity<InstancesModel>, id: String, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&model, |this, _, cx| {
            this.sync(cx);
            cx.notify();
        });
        let mut this = Self {
            model,
            id,
            row: Row::Minecraft,
            java: None,
            java_mc: None,
            java_revision: Revision::default(),
            java_task: Task::ready(()),
            tags: None,
            tags_key: None,
            tags_revision: Revision::default(),
            tags_task: Task::ready(()),
            _observe,
        };
        this.sync(cx);
        this
    }

    pub fn instance_id(&self) -> &str {
        &self.id
    }

    /// The selected row.
    pub fn row(&self) -> Row {
        self.row
    }

    /// The Java major version the Minecraft row shows, once known.
    pub fn java(&self) -> Option<u32> {
        self.java
    }

    /// Selects `row` (the loader row only while there is a loader).
    pub fn select(&mut self, row: Row, cx: &mut Context<Self>) {
        if self.row != row {
            self.row = row;
            cx.notify();
        }
    }

    /// The instance's config as listed.
    fn config(&self, cx: &App) -> Option<InstanceConfig> {
        match self.model.read(cx).entry(&self.id) {
            Some(Entry::Healthy(instance)) => Some(instance.config.clone()),
            _ => None,
        }
    }

    /// Reads what depends on the instance's components again when they
    /// changed: the Java version and the loader's tags.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.config(cx) else {
            return;
        };
        let mc = config.mc_version().to_string();
        let loader = config.loader().map(|(k, c)| (k, c.version.clone()));
        if loader.is_none() && self.row == Row::Loader {
            self.row = Row::Minecraft;
        }
        if self.java_mc.as_deref() != Some(mc.as_str()) {
            self.java_mc = Some(mc.clone());
            self.java = None;
            let request = self.java_revision.start();
            let versions = Core::global(cx).data.versions();
            let job = {
                let mc = mc.clone();
                run_blocking(cx, move || overview::required_java(&versions, &mc))
            };
            self.java_task = cx.spawn(async move |this, cx| {
                let result = job.await;
                this.update(cx, |this, cx| {
                    if this.java_revision.is_current(request)
                        && let Ok(java) = result
                    {
                        this.java = java;
                        cx.notify();
                    }
                })
                .ok();
            });
        }
        let key = loader.map(|(kind, _)| (kind, mc));
        if key != self.tags_key {
            self.tags_key = key.clone();
            self.tags = None;
            let request = self.tags_revision.start();
            self.tags_task = Task::ready(());
            if let Some((kind, mc)) = key {
                let request_list = Core::global(cx).catalog.loader_versions(kind, mc);
                let job = run(cx, request_list);
                self.tags_task = cx.spawn(async move |this, cx| {
                    let result = job.await;
                    this.update(cx, |this, cx| {
                        if !this.tags_revision.is_current(request) {
                            return;
                        }
                        // Failed or empty: no tags.
                        if let Ok(Ok(list)) = result {
                            this.tags = Some(LoaderTags::of(&list));
                            cx.notify();
                        }
                    })
                    .ok();
                });
            }
        }
    }

    /// Why the changes are disabled, or `None` when they are not.
    fn busy_reason(&self, cx: &App) -> Option<String> {
        busy_reason(self.model.read(cx), &self.id)
    }

    fn tag(id: &'static str, text: String, primary: bool) -> gpui_kit::AnyElement {
        let tag = if primary {
            Tag::primary().outline()
        } else {
            Tag::secondary()
        };
        div()
            .id(id)
            .test_support()
            .aria_label(text.clone())
            .child(tag.xsmall().child(text))
            .into_any_element()
    }

    /// One entry of the component list.
    fn render_row(&self, spec: RowSpec, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (hover, active) = (theme.list_hover, theme.list_active);
        let muted = theme.muted_foreground;
        let row = spec.row;
        let selected = self.row == row;
        h_flex()
            .id(spec.id)
            .test_support()
            .aria_selected(selected)
            .w_full()
            .px(GUTTER)
            .py(rems(0.625))
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .when(selected, |r| r.bg(active))
            .when(!selected, |r| r.hover(move |s| s.bg(hover)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(rems(0.25))
                    .child(spec.name)
                    .child(
                        div()
                            .text_size(TEXT_SECONDARY)
                            .text_color(muted)
                            .child(spec.detail),
                    )
                    .children(spec.extra.map(|(id, text)| {
                        div()
                            .id(id)
                            .test_support()
                            .aria_label(text.clone())
                            .text_size(TEXT_SECONDARY)
                            .text_color(muted)
                            .child(text)
                    })),
            )
            .child(h_flex().gap_1().children(spec.tags))
            .on_click(cx.listener(move |this, _, _, cx| this.select(row, cx)))
    }

    fn render_actions(
        &self,
        mc: &str,
        loader: Option<(LoaderKind, &str)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let reason = self.busy_reason(cx);
        let busy = reason.is_some();
        let theme = cx.theme();
        let (model, id) = (self.model.clone(), self.id.clone());
        let action = |key: &'static str, label: String| {
            let button = Button::new(key).outline().label(label).disabled(busy);
            match &reason {
                Some(reason) => button.tooltip(reason.clone()),
                None => button,
            }
        };
        let title = match (self.row, loader) {
            (Row::Loader, Some((kind, _))) => kind.display_name(),
            _ => t!("versions.minecraft").to_string(),
        };
        let buttons = match (self.row, loader) {
            (Row::Loader, Some((kind, version))) => {
                let (version, mc) = (version.to_string(), mc.to_string());
                vec![
                    action(
                        "versions-change-loader-version",
                        t!("versions.change_loader_version").to_string(),
                    )
                    .on_click({
                        let (model, id, version, mc) =
                            (model.clone(), id.clone(), version.clone(), mc.clone());
                        move |_, window, cx| {
                            open_loader_version(
                                model.clone(),
                                id.clone(),
                                kind,
                                version.clone(),
                                mc.clone(),
                                window,
                                cx,
                            )
                        }
                    }),
                    action(
                        "versions-switch-loader",
                        t!("versions.switch_loader").to_string(),
                    )
                    .on_click({
                        let (model, id, mc) = (model.clone(), id.clone(), mc.clone());
                        move |_, window, cx| {
                            open_switch(model.clone(), id.clone(), kind, mc.clone(), window, cx)
                        }
                    }),
                    action("versions-repair", t!("versions.repair").to_string()).on_click({
                        let (model, id) = (model.clone(), id.clone());
                        move |_, _, cx| {
                            model.update(cx, |m, cx| reinstall(m, &id, cx));
                        }
                    }),
                    action(
                        "versions-remove-loader",
                        t!("versions.remove_loader").to_string(),
                    )
                    .danger()
                    .on_click({
                        let (model, id, version) = (model.clone(), id.clone(), version.clone());
                        move |_, window, cx| {
                            open_remove(
                                model.clone(),
                                id.clone(),
                                format!("{} {version}", kind.display_name()),
                                window,
                                cx,
                            )
                        }
                    }),
                ]
            }
            _ => vec![
                action(
                    "versions-change-mc",
                    t!("versions.change_game_version").to_string(),
                )
                .on_click({
                    let (model, id, mc) = (model.clone(), id.clone(), mc.to_string());
                    move |_, window, cx| {
                        open_game_version(model.clone(), id.clone(), mc.clone(), window, cx)
                    }
                }),
            ],
        };
        v_flex()
            .id("versions-actions")
            .test_support()
            .flex_none()
            .w(rems(ACTIONS_W))
            .p(GUTTER)
            .gap_2()
            .border_l_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(TEXT_PRIMARY)
                    .strong(FontWeight::MEDIUM, cx)
                    .text_color(theme.foreground)
                    .child(t!("versions.actions_for", component = title).to_string()),
            )
            .children(buttons)
            .children(reason.map(|reason| {
                div()
                    .id("versions-reason")
                    .test_support()
                    .aria_label(reason.clone())
                    .text_size(TEXT_SECONDARY)
                    .text_color(theme.muted_foreground)
                    .child(reason)
            }))
    }
}

/// What a row of the component list shows.
struct RowSpec {
    row: Row,
    id: &'static str,
    name: gpui_kit::AnyElement,
    /// The version.
    detail: String,
    /// A muted line below it, with its element id.
    extra: Option<(&'static str, String)>,
    tags: Vec<gpui_kit::AnyElement>,
}

impl Render for VersionsTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(config) = self.config(cx) else {
            return div().id("versions-tab").into_any_element();
        };
        let mc = config.mc_version().to_string();
        let loader = config.loader().map(|(k, c)| (k, c.version.clone()));
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let name = |text: String| {
            div()
                .text_size(TEXT_PRIMARY)
                .strong(FontWeight::MEDIUM, cx)
                .text_color(fg)
                .child(text)
                .into_any_element()
        };
        let java = (self.java).map(|v| {
            (
                "versions-java",
                t!("versions.needs_java", version = v).to_string(),
            )
        });
        let mc_row = self.render_row(
            RowSpec {
                row: Row::Minecraft,
                id: "versions-row-minecraft",
                name: name(t!("versions.minecraft").to_string()),
                detail: mc.clone(),
                extra: java,
                tags: Vec::new(),
            },
            cx,
        );
        let loader_row = match loader.as_ref() {
            None => None,
            Some((kind, version)) => {
                let color = loader_color(&config, cx);
                let mut tags = Vec::new();
                if let Some(found) = &self.tags {
                    if found.recommended.as_deref() == Some(version.as_str()) {
                        let text = t!("versions.recommended").to_string();
                        tags.push(Self::tag("versions-tag-recommended", text, true));
                    }
                    if found.latest.as_deref() == Some(version.as_str()) {
                        let text = t!("versions.latest").to_string();
                        tags.push(Self::tag("versions-tag-latest", text, false));
                    }
                }
                let chip = div()
                    .flex_none()
                    .self_start()
                    .px_1p5()
                    .rounded_sm()
                    .bg(color.opacity(0.14))
                    .text_color(color)
                    .text_size(TEXT_PRIMARY)
                    .child(kind.display_name())
                    .into_any_element();
                Some(self.render_row(
                    RowSpec {
                        row: Row::Loader,
                        id: "versions-row-loader",
                        name: chip,
                        detail: version.clone(),
                        extra: None,
                        tags,
                    },
                    cx,
                ))
            }
        };
        let install = loader.is_none().then(|| {
            let busy = self.busy_reason(cx);
            let (model, id, mc) = (self.model.clone(), self.id.clone(), mc.clone());
            let button = Button::new("versions-install")
                .ghost()
                .label(t!("versions.install_loader").to_string())
                .disabled(busy.is_some())
                .on_click(move |_, window, cx| {
                    open_install(model.clone(), id.clone(), mc.clone(), window, cx)
                });
            let (button, note) = match busy {
                Some(reason) => (
                    button.tooltip(reason.clone()),
                    ("versions-install-reason", reason),
                ),
                None => (
                    button,
                    ("versions-hint", t!("versions.vanilla_hint").to_string()),
                ),
            };
            v_flex()
                .px(GUTTER)
                .py_2()
                .gap_1()
                .items_start()
                .child(button)
                .child(
                    div()
                        .id(note.0)
                        .test_support()
                        .aria_label(note.1.clone())
                        .text_size(TEXT_SECONDARY)
                        .text_color(muted)
                        .child(note.1),
                )
        });
        let loader_ref = loader.as_ref().map(|(k, v)| (*k, v.as_str()));
        let actions = self.render_actions(&mc, loader_ref, cx);
        h_flex()
            .id("versions-tab")
            .test_support()
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(mc_row)
                    .children(loader_row)
                    .children(install),
            )
            .child(actions)
            .into_any_element()
    }
}

// --- operations ---------------------------------------------------------

/// What a version change ended with.
enum Changed {
    Done(OpOutcome),
    /// The loader does not support the new Minecraft version.
    Incompatible {
        mc: String,
        kind: LoaderKind,
        version: String,
        can_replace: bool,
    },
}

/// Why instance `id` takes no change now: an operation is in flight, or
/// its game runs.
fn busy_reason(model: &InstancesModel, id: &str) -> Option<String> {
    if model.is_op_running(id) {
        Some(t!("versions.busy_operation").to_string())
    } else if model.is_busy(id) {
        Some(t!("versions.busy_running").to_string())
    } else {
        None
    }
}

/// Shows `text` as a warning once the model is not being updated.
fn warn_later(texts: Vec<String>, cx: &mut Context<InstancesModel>) {
    cx.spawn(async move |_, cx| {
        cx.update(|cx| {
            in_main_window(cx, move |window, cx| {
                for text in texts {
                    window.push_notification(Notification::warning(text), cx);
                }
            })
        })
    })
    .detach();
}

/// Runs the change `call` as operation `kind` of `id`; `retry` starts it
/// again. `on_done` runs with the outcome. A busy instance takes no change:
/// the user is told why.
fn start(
    model: &mut InstancesModel,
    id: &str,
    kind: OpKind,
    call: impl FnOnce(
        Arc<dyn VersionOps>,
        ProgressSink,
    ) -> BoxFuture<'static, Result<Changed, LoaderError>>
    + Send
    + 'static,
    retry: RetryAction,
    cx: &mut Context<InstancesModel>,
) {
    let ops = Core::global(cx).version_ops.clone();
    let started = model.start_op(
        id,
        kind,
        move |progress| async move { call(ops, progress).await.map_err(OpError::from) }.boxed(),
        {
            let key = id.to_string();
            move |model, changed, cx| finished(model, &key, changed, cx)
        },
        Some(retry),
        cx,
    );
    if !started {
        let name = model.display_name(id).unwrap_or_else(|| id.to_string());
        let reason =
            busy_reason(model, id).unwrap_or_else(|| t!("versions.busy_operation").to_string());
        warn_later(
            vec![t!("versions.refused", name = name, reason = reason).to_string()],
            cx,
        );
    }
}

fn done(
    future: BoxFuture<'static, Result<OpOutcome, LoaderError>>,
) -> BoxFuture<'static, Result<Changed, LoaderError>> {
    async move { future.await.map(Changed::Done) }.boxed()
}

/// Installs `version` of loader `kind` into `id`; `replace` allows
/// replacing a loader of another kind (and then warns about mods).
fn install(
    model: &mut InstancesModel,
    id: &str,
    kind: LoaderKind,
    version: String,
    replace: bool,
    cx: &mut Context<InstancesModel>,
) {
    let retry: RetryAction = {
        let (id, version) = (id.to_string(), version.clone());
        Rc::new(move |m, cx| install(m, &id, kind, version.clone(), replace, cx))
    };
    let target = id.to_string();
    start(
        model,
        id,
        OpKind::InstallLoader,
        move |ops, progress| {
            done(ops.install_loader(target, kind, VersionPick::Exact(version), replace, progress))
        },
        retry,
        cx,
    );
}

/// Changes the Minecraft version of `id` to `mc`; `action` is what to do
/// with a loader that does not support it.
fn change_mc(
    model: &mut InstancesModel,
    id: &str,
    mc: String,
    action: Option<LoaderAction>,
    cx: &mut Context<InstancesModel>,
) {
    let retry: RetryAction = {
        let (id, mc) = (id.to_string(), mc.clone());
        Rc::new(move |m, cx| change_mc(m, &id, mc.clone(), action, cx))
    };
    let target = id.to_string();
    start(
        model,
        id,
        OpKind::ChangeVersion,
        move |ops, progress| {
            async move {
                match ops
                    .set_mc_version(target, mc.clone(), action, progress)
                    .await
                {
                    Ok(outcome) => Ok(Changed::Done(outcome)),
                    // Only asked when nothing was decided yet.
                    Err(LoaderError::Incompatible {
                        kind,
                        version,
                        can_replace,
                    }) if action.is_none() => Ok(Changed::Incompatible {
                        mc,
                        kind,
                        version,
                        can_replace,
                    }),
                    Err(error) => Err(error),
                }
            }
            .boxed()
        },
        retry,
        cx,
    );
}

/// Reinstalls the loader of `id` (repair).
fn reinstall(model: &mut InstancesModel, id: &str, cx: &mut Context<InstancesModel>) {
    let retry: RetryAction = {
        let id = id.to_string();
        Rc::new(move |m, cx| reinstall(m, &id, cx))
    };
    let target = id.to_string();
    start(
        model,
        id,
        OpKind::Reinstall,
        move |ops, progress| done(ops.reinstall_loader(target, progress)),
        retry,
        cx,
    );
}

/// Removes the loader of `id`.
fn remove(model: &mut InstancesModel, id: &str, cx: &mut Context<InstancesModel>) {
    let retry: RetryAction = {
        let id = id.to_string();
        Rc::new(move |m, cx| remove(m, &id, cx))
    };
    let target = id.to_string();
    start(
        model,
        id,
        OpKind::RemoveLoader,
        move |ops, _| done(ops.remove_loader(target)),
        retry,
        cx,
    );
}

/// Shows the notices of a finished change, or asks what to do about an
/// incompatible loader.
fn finished(_: &mut InstancesModel, id: &str, changed: Changed, cx: &mut Context<InstancesModel>) {
    match changed {
        Changed::Done(outcome) => {
            let notices: Vec<String> = outcome.notices.iter().map(ToString::to_string).collect();
            if notices.is_empty() {
                return;
            }
            warn_later(notices, cx);
        }
        Changed::Incompatible {
            mc,
            kind,
            version,
            can_replace,
        } => {
            let (model, id) = (cx.entity(), id.to_string());
            // The model is being updated: open the dialog once it is not.
            cx.spawn(async move |_, cx| {
                cx.update(|cx| {
                    in_main_window(cx, move |window, cx| {
                        let ask = Incompatible {
                            id,
                            mc,
                            kind,
                            version,
                            can_replace,
                        };
                        open_incompatible(model, ask, window, cx)
                    })
                })
            })
            .detach();
        }
    }
}

/// Runs `f` on the window that has the keyboard (else the first one).
fn in_main_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App) + 'static) {
    let handle = cx.active_window().or_else(|| cx.windows().first().copied());
    if let Some(handle) = handle {
        handle.update(cx, |_, window, cx| f(window, cx)).ok();
    }
}

// --- dialogs ------------------------------------------------------------

/// What `set_mc_version` found out about the loader.
struct Incompatible {
    id: String,
    mc: String,
    kind: LoaderKind,
    version: String,
    can_replace: bool,
}

fn open_incompatible(
    model: Entity<InstancesModel>,
    ask: Incompatible,
    window: &mut Window,
    cx: &mut App,
) {
    let Incompatible {
        id,
        mc,
        kind,
        version,
        can_replace,
    } = ask;
    window.open_dialog(cx, move |dialog, window, cx| {
        let title = SharedString::from(t!(
            "versions.incompatible.title",
            loader = format!("{} {version}", kind.display_name()),
            mc = mc
        ));
        let body = if can_replace {
            t!(
                "versions.incompatible.body_replace",
                kind = kind.display_name(),
                mc = mc
            )
        } else {
            t!(
                "versions.incompatible.body_remove",
                kind = kind.display_name(),
                mc = mc
            )
        }
        .to_string();
        // "Use recommended" (when possible), "Remove loader", "Cancel"; each
        // runs the change again with its action.
        let rerun = |action: LoaderAction| {
            let (model, id, mc) = (model.clone(), id.clone(), mc.clone());
            move |_: &_, window: &mut Window, cx: &mut App| {
                window.close_dialog(cx);
                model.update(cx, |m, cx| change_mc(m, &id, mc.clone(), Some(action), cx));
            }
        };
        let footer = DialogFooter::new()
            .child(
                Button::new("incompatible-cancel")
                    .label(t!("versions.incompatible.cancel").to_string())
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("incompatible-remove")
                    .danger()
                    .outline()
                    .label(t!("versions.incompatible.remove").to_string())
                    .on_click(rerun(LoaderAction::Remove)),
            )
            .when(can_replace, |f| {
                f.child(
                    Button::new("incompatible-replace")
                        .primary()
                        .label(t!("versions.incompatible.replace").to_string())
                        .on_click(rerun(LoaderAction::Replace)),
                )
            });
        dialog
            .w(rems(DIALOG_W).to_pixels(window.rem_size()))
            .title(
                div()
                    .id("incompatible-title")
                    .aria_label(title.clone())
                    .test_support()
                    .child(title),
            )
            .child(
                div()
                    .text_size(TEXT_PRIMARY)
                    .text_color(cx.theme().foreground)
                    .child(body),
            )
            .footer(footer)
    });
}

/// What a dialog's confirming button (or Enter) does.
type Commit = Rc<dyn Fn(&mut Window, &mut App)>;
/// Whether a dialog's confirming button is enabled.
type Ready = Rc<dyn Fn(&App) -> bool>;

/// "取消" and the confirming button of a picker dialog (an entity, so the
/// button follows the picker and the instance's busy state).
struct PickFooter {
    id: &'static str,
    label: String,
    ready: Ready,
    commit: Commit,
    _watch: Vec<Subscription>,
}

impl Render for PickFooter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = (self.ready)(cx);
        let commit = self.commit.clone();
        DialogFooter::new()
            .child(
                Button::new("versions-dialog-cancel")
                    .label(t!("versions.cancel").to_string())
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new(self.id)
                    .primary()
                    .label(self.label.clone())
                    .disabled(!ready)
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        commit(window, cx);
                    }),
            )
    }
}

/// A dialog hosting a picker: the confirming button is enabled while
/// `ready` holds and the instance is not busy.
struct PickerDialog<T: 'static> {
    model: Entity<InstancesModel>,
    /// The instance the change is for.
    instance: String,
    title_id: &'static str,
    title: String,
    picker: Entity<T>,
    content: AnyView,
    /// The picker's list: Enter commits only from there.
    list: FocusHandle,
    confirm_id: &'static str,
    confirm: String,
    ready: Ready,
    commit: Commit,
}

impl<T: 'static> PickerDialog<T> {
    /// Opens the dialog. Enter commits only from the list: on any other
    /// control it is that control's own key (a button presses itself), so
    /// the action goes on to its key listeners. Like the button, Enter
    /// closes the dialog first and then commits, because committing may
    /// open another dialog.
    fn open(self, window: &mut Window, cx: &mut App) {
        let PickerDialog {
            model,
            instance,
            title_id,
            title,
            picker,
            content,
            list,
            confirm_id,
            confirm,
            ready,
            commit,
        } = self;
        let ready: Ready = {
            let (model, ready) = (model.clone(), ready);
            Rc::new(move |cx| !model.read(cx).is_busy(&instance) && ready(cx))
        };
        let footer = {
            let (ready, commit) = (ready.clone(), commit.clone());
            cx.new(|cx| PickFooter {
                id: confirm_id,
                label: confirm,
                ready,
                commit,
                _watch: vec![
                    cx.observe(&picker, |_, _, cx| cx.notify()),
                    cx.observe(&model, |_, _, cx| cx.notify()),
                ],
            })
        };
        window.open_dialog(cx, move |dialog, window, _| {
            let title = SharedString::from(title.clone());
            let (commit, ready, list) = (commit.clone(), ready.clone(), list.clone());
            dialog
                .w(rems(DIALOG_W).to_pixels(window.rem_size()))
                .title(
                    div()
                        .id(title_id)
                        .aria_label(title.clone())
                        .test_support()
                        .child(title),
                )
                .child(content.clone())
                .footer(footer.clone())
                .on_ok(move |_, window, cx| {
                    if !list.is_focused(window) {
                        cx.propagate();
                        return false;
                    }
                    if !ready(cx) {
                        return false;
                    }
                    window.close_dialog(cx);
                    commit(window, cx);
                    false
                })
        });
    }
}

fn name_of(model: &Entity<InstancesModel>, id: &str, cx: &App) -> String {
    (model.read(cx).display_name(id)).unwrap_or_else(|| id.to_string())
}

/// "更改游戏版本…": pick a Minecraft version, then change to it.
fn open_game_version(
    model: Entity<InstancesModel>,
    id: String,
    current: String,
    window: &mut Window,
    cx: &mut App,
) {
    let picker = cx.new(|cx| VersionPicker::new(Some(current.clone()), window, cx));
    let list = picker.read(cx).list_focus().clone();
    let title = t!(
        "versions.game_version_title",
        name = name_of(&model, &id, cx)
    )
    .to_string();
    let ready: Ready = {
        let picker = picker.clone();
        Rc::new(move |cx| picker.read(cx).chosen().is_some_and(|c| c != current))
    };
    let commit: Commit = {
        let (picker, model, id) = (picker.clone(), model.clone(), id.clone());
        Rc::new(move |_, cx| {
            if let Some(mc) = picker.read(cx).chosen().map(str::to_owned) {
                model.update(cx, |m, cx| change_mc(m, &id, mc, None, cx));
            }
        })
    };
    PickerDialog {
        model,
        instance: id,
        title_id: "versions-dialog-title",
        title,
        content: picker.clone().into(),
        picker,
        list,
        confirm_id: "versions-dialog-confirm",
        confirm: t!("versions.change").to_string(),
        ready,
        commit,
    }
    .open(window, cx);
}

/// "更换版本…": pick another version of the installed loader.
fn open_loader_version(
    model: Entity<InstancesModel>,
    id: String,
    kind: LoaderKind,
    current: String,
    mc: String,
    window: &mut Window,
    cx: &mut App,
) {
    let picker = cx.new(|cx| {
        let mut picker = LoaderPicker::new(cx);
        picker.set_mc(Some(mc), cx);
        picker.lock_kind(kind, cx);
        picker.choose_version(&current, cx);
        picker
    });
    let list = picker.read(cx).list_focus().clone();
    let title = t!(
        "versions.loader_version_title",
        name = name_of(&model, &id, cx),
        loader = kind.display_name()
    )
    .to_string();
    let ready: Ready = {
        let picker = picker.clone();
        Rc::new(move |cx| picker.read(cx).version().is_some_and(|v| v != current))
    };
    let commit: Commit = {
        let (picker, model, id) = (picker.clone(), model.clone(), id.clone());
        Rc::new(move |_, cx| {
            if let Some(version) = picker.read(cx).version().map(str::to_owned) {
                model.update(cx, |m, cx| install(m, &id, kind, version, false, cx));
            }
        })
    };
    PickerDialog {
        model,
        instance: id,
        title_id: "versions-dialog-title",
        title,
        content: picker.clone().into(),
        picker,
        list,
        confirm_id: "versions-dialog-confirm",
        confirm: t!("versions.change").to_string(),
        ready,
        commit,
    }
    .open(window, cx);
}

/// "更换为其他加载器…": pick the other loader and its version, then ask
/// before switching.
fn open_switch(
    model: Entity<InstancesModel>,
    id: String,
    current: LoaderKind,
    mc: String,
    window: &mut Window,
    cx: &mut App,
) {
    let picker = cx.new(|cx| {
        let mut picker = LoaderPicker::new(cx);
        // Neither "无" (that is removing) nor the installed kind (that is
        // changing its version) is a switch.
        picker.hide_kind(None);
        picker.hide_kind(Some(current));
        picker.set_mc(Some(mc), cx);
        picker
    });
    let list = picker.read(cx).list_focus().clone();
    let title = t!("versions.switch_title", name = name_of(&model, &id, cx)).to_string();
    let ready: Ready = {
        let picker = picker.clone();
        Rc::new(move |cx| {
            let picker = picker.read(cx);
            picker.kind().is_some_and(|k| k != current) && picker.version().is_some()
        })
    };
    let commit: Commit = {
        let (picker, model, id) = (picker.clone(), model.clone(), id.clone());
        Rc::new(move |window, cx| {
            let picker = picker.read(cx);
            if let (Some(kind), Some(version)) = (picker.kind(), picker.version()) {
                let version = version.to_string();
                open_switch_confirm(
                    model.clone(),
                    id.clone(),
                    current,
                    kind,
                    version,
                    window,
                    cx,
                );
            }
        })
    };
    PickerDialog {
        model,
        instance: id,
        title_id: "versions-dialog-title",
        title,
        content: picker.clone().into(),
        picker,
        list,
        confirm_id: "versions-dialog-confirm",
        confirm: t!("versions.switch").to_string(),
        ready,
        commit,
    }
    .open(window, cx);
}

/// `把加载器换成 <新>？`: mods made for the old loader may not work.
fn open_switch_confirm(
    model: Entity<InstancesModel>,
    id: String,
    old: LoaderKind,
    kind: LoaderKind,
    version: String,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let title = SharedString::from(t!(
            "versions.switch_confirm_title",
            loader = kind.display_name()
        ));
        let (model, id, version) = (model.clone(), id.clone(), version.clone());
        alert
            .title(
                div()
                    .id("versions-confirm-title")
                    .aria_label(title.clone())
                    .test_support()
                    .child(title),
            )
            .description(
                t!(
                    "versions.switch_confirm_body",
                    old = old.display_name(),
                    loader = kind.display_name()
                )
                .to_string(),
            )
            .confirm()
            .ok_text(t!("versions.switch_confirm").to_string())
            .cancel_text(t!("versions.cancel").to_string())
            .on_ok(move |_, _, cx| {
                model.update(cx, |m, cx| install(m, &id, kind, version.clone(), true, cx));
                true
            })
    });
}

/// "移除加载器…": asks first.
fn open_remove(
    model: Entity<InstancesModel>,
    id: String,
    loader: String,
    window: &mut Window,
    cx: &mut App,
) {
    let name = name_of(&model, &id, cx);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let title = SharedString::from(t!("versions.remove_title", name = name, loader = loader));
        let (model, id) = (model.clone(), id.clone());
        alert
            .title(
                div()
                    .id("versions-confirm-title")
                    .aria_label(title.clone())
                    .test_support()
                    .child(title),
            )
            .description(t!("versions.remove_body").to_string())
            .confirm()
            .ok_text(t!("versions.remove_confirm").to_string())
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("versions.cancel").to_string())
            .on_ok(move |_, _, cx| {
                model.update(cx, |m, cx| remove(m, &id, cx));
                true
            })
    });
}

/// "＋ 安装加载器…": pick a loader and its version for a vanilla instance.
fn open_install(
    model: Entity<InstancesModel>,
    id: String,
    mc: String,
    window: &mut Window,
    cx: &mut App,
) {
    let picker = cx.new(|cx| {
        let mut picker = LoaderPicker::new(cx);
        picker.set_mc(Some(mc), cx);
        picker
    });
    let list = picker.read(cx).list_focus().clone();
    let title = t!("versions.install_title", name = name_of(&model, &id, cx)).to_string();
    let ready: Ready = {
        let picker = picker.clone();
        Rc::new(move |cx| {
            let picker = picker.read(cx);
            picker.kind().is_some() && picker.version().is_some()
        })
    };
    let commit: Commit = {
        let (picker, model, id) = (picker.clone(), model.clone(), id.clone());
        Rc::new(move |_, cx| {
            let picker = picker.read(cx);
            if let (Some(kind), Some(version)) = (picker.kind(), picker.version()) {
                let version = version.to_string();
                model.update(cx, |m, cx| install(m, &id, kind, version, false, cx));
            }
        })
    };
    PickerDialog {
        model,
        instance: id,
        title_id: "versions-dialog-title",
        title,
        content: picker.clone().into(),
        picker,
        list,
        confirm_id: "versions-dialog-confirm",
        confirm: t!("versions.install").to_string(),
        ready,
        commit,
    }
    .open(window, cx);
}
