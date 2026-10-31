//! The selected instance's detail: header (icon, name, "加载器 · 版本 ·
//! Java · 上次游玩", the operation or run status, 启动 / 强制关闭…), the
//! tab strip with its teal underline, then the tab's content.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::base::{Tab as TabElement, Tabs};
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Rems, Render, StatefulInteractiveElement, Styled, Subscription, Task,
    TestSupportExt, Window, div, px, rems,
};
use patina_core::instance::Instance;
use rust_i18n::t;

use super::model::{Entry, InstancesModel};
use super::overview::{self, OverviewStats};
use super::versions_tab::VersionsTab;
use super::{
    HEADER_ICON, HEADER_TITLE, icon_for, instance_icon, loader_color, loader_full, now_secs,
    time_ago,
};
use crate::core_bridge::{Core, Revision, run_blocking};
use crate::fonts::Strong;
use crate::launch::{LogView, OpState, RunState, stop_dialog};
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_SECONDARY};
use crate::shell::placeholder;
use crate::theme::ProductColors;

const TABS_H: Rems = Rems(2.25);

/// A tab of the detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetailTab {
    #[default]
    Overview,
    Versions,
    Mods,
    ResourcePacks,
    Shaders,
    Worlds,
    Settings,
    Log,
}

impl DetailTab {
    pub const ALL: [DetailTab; 8] = [
        DetailTab::Overview,
        DetailTab::Versions,
        DetailTab::Mods,
        DetailTab::ResourcePacks,
        DetailTab::Shaders,
        DetailTab::Worlds,
        DetailTab::Settings,
        DetailTab::Log,
    ];

    pub fn label(self) -> String {
        match self {
            DetailTab::Overview => t!("instances.tabs.overview"),
            DetailTab::Versions => t!("instances.tabs.versions"),
            DetailTab::Mods => t!("instances.tabs.mods"),
            DetailTab::ResourcePacks => t!("instances.tabs.resource_packs"),
            DetailTab::Shaders => t!("instances.tabs.shaders"),
            DetailTab::Worlds => t!("instances.tabs.worlds"),
            DetailTab::Settings => t!("instances.tabs.settings"),
            DetailTab::Log => t!("instances.tabs.log"),
        }
        .to_string()
    }

    /// Element id of the tab.
    pub fn id(self) -> &'static str {
        match self {
            DetailTab::Overview => "tab-overview",
            DetailTab::Versions => "tab-versions",
            DetailTab::Mods => "tab-mods",
            DetailTab::ResourcePacks => "tab-resource-packs",
            DetailTab::Shaders => "tab-shaders",
            DetailTab::Worlds => "tab-worlds",
            DetailTab::Settings => "tab-settings",
            DetailTab::Log => "tab-log",
        }
    }

    fn icon(self) -> Icon {
        match self {
            DetailTab::Overview => Icon::new(IconName::LayoutDashboard),
            DetailTab::Versions => Icon::new(Lucide::Layers),
            DetailTab::Mods => Icon::new(Lucide::Puzzle),
            DetailTab::ResourcePacks => Icon::new(Lucide::Image),
            DetailTab::Shaders => Icon::new(Lucide::SunMedium),
            DetailTab::Worlds => Icon::new(Lucide::Earth),
            DetailTab::Settings => Icon::new(IconName::Settings2),
            DetailTab::Log => Icon::new(Lucide::ScrollText),
        }
    }
}

/// Where the overview's stats stand for the selected instance.
#[derive(Debug, Default)]
struct StatsState {
    /// The instance they are (being) read for.
    id: Option<String>,
    stats: Option<OverviewStats>,
    /// The Java version `id` needs, from a quick first read that lands
    /// before the disk walk, so the header settles early.
    java: Option<u32>,
    /// The model generation they were started for.
    generation: u64,
    /// A read is running for `id`.
    reading: bool,
    /// The list changed while reading; read again afterwards.
    again: bool,
    revision: Revision,
}

/// The detail of the selected healthy instance (an entity). Owns the tab
/// and the overview's stats, which it reads in the background whenever the
/// selection or the list changes.
pub struct DetailView {
    model: Entity<InstancesModel>,
    tab: DetailTab,
    stats: StatsState,
    stats_task: Task<()>,
    /// The log tab of the selected instance, made when it is first shown.
    log: Option<Entity<LogView>>,
    /// The versions tab of the selected instance, made with the selection.
    versions: Option<Entity<VersionsTab>>,
    _observe: Subscription,
}

impl DetailView {
    pub fn new(model: Entity<InstancesModel>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&model, |this, _, cx| this.sync_stats(cx));
        let mut this = Self {
            model,
            tab: DetailTab::default(),
            stats: StatsState::default(),
            stats_task: Task::ready(()),
            log: None,
            versions: None,
            _observe,
        };
        this.sync_stats(cx);
        this
    }

    pub fn tab(&self) -> DetailTab {
        self.tab
    }

    /// The versions tab's view of the selected instance.
    pub fn versions_view(&self) -> Option<&Entity<VersionsTab>> {
        self.versions.as_ref()
    }

    /// The log tab's view, once the tab has been shown for the selection.
    pub fn log_view(&self) -> Option<&Entity<LogView>> {
        self.log.as_ref()
    }

    pub fn set_tab(&mut self, tab: DetailTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            cx.notify();
        }
    }

    /// The overview's stats of instance `id`, once read.
    pub fn stats(&self, id: &str) -> Option<&OverviewStats> {
        let s = &self.stats;
        s.stats.as_ref().filter(|_| s.id.as_deref() == Some(id))
    }

    /// The Java major version instance `id` needs, once known.
    pub fn java(&self, id: &str) -> Option<u32> {
        let s = &self.stats;
        s.java.filter(|_| s.id.as_deref() == Some(id))
    }

    /// Makes the log view follow the selection.
    fn sync_log(&mut self, cx: &mut Context<Self>) {
        let id = match self.model.read(cx).selected_entry() {
            Some(Entry::Healthy(instance)) => instance.id.clone(),
            _ => return,
        };
        let current = self
            .log
            .as_ref()
            .is_some_and(|v| v.read(cx).instance_id() == id);
        if !current {
            let model = self.model.clone();
            self.log = Some(cx.new(|cx| LogView::new(model, id, cx)));
        }
    }

    /// Makes the versions tab follow the selection.
    fn sync_versions(&mut self, cx: &mut Context<Self>) {
        let id = match self.model.read(cx).selected_entry() {
            Some(Entry::Healthy(instance)) => instance.id.clone(),
            _ => return,
        };
        let current = self
            .versions
            .as_ref()
            .is_some_and(|v| v.read(cx).instance_id() == id);
        if !current {
            let model = self.model.clone();
            self.versions = Some(cx.new(|cx| VersionsTab::new(model, id, cx)));
        }
    }

    /// Starts reading the stats when the selected instance or the list
    /// changed. One read at a time per instance; a newer one for another
    /// instance makes the older result stale.
    fn sync_stats(&mut self, cx: &mut Context<Self>) {
        self.sync_log(cx);
        self.sync_versions(cx);
        let model = self.model.read(cx);
        let Some(Entry::Healthy(instance)) = model.selected_entry() else {
            cx.notify();
            return;
        };
        let generation = model.generation();
        let same = self.stats.id.as_deref() == Some(instance.id.as_str());
        if same && self.stats.generation == generation {
            cx.notify();
            return;
        }
        if same && self.stats.reading {
            self.stats.again = true;
            return;
        }
        let instance = instance.clone();
        if !same {
            self.stats.id = Some(instance.id.clone());
            self.stats.stats = None;
            self.stats.java = None;
        }
        self.stats.generation = generation;
        self.read_stats(instance, cx);
        cx.notify();
    }

    fn read_stats(&mut self, instance: Instance, cx: &mut Context<Self>) {
        let request = self.stats.revision.start();
        self.stats.reading = true;
        self.stats.again = false;
        let versions = Core::global(cx).data.versions();
        // The Java version is one small file: read it first, on its own.
        let quick = {
            let (versions, mc) = (versions.clone(), instance.config.mc_version().to_string());
            run_blocking(cx, move || overview::required_java(&versions, &mc))
        };
        let job = run_blocking(cx, move || OverviewStats::compute(&instance, &versions));
        self.stats_task = cx.spawn(async move |this, cx| {
            if let Ok(java) = quick.await {
                this.update(cx, |this, cx| {
                    if this.stats.revision.is_current(request) && this.stats.java != java {
                        this.stats.java = java;
                        cx.notify();
                    }
                })
                .ok();
            }
            let result = job.await;
            this.update(cx, |this, cx| {
                if !this.stats.revision.is_current(request) {
                    return;
                }
                this.stats.reading = false;
                match result {
                    Ok(stats) => {
                        this.stats.java = stats.java;
                        this.stats.stats = Some(stats);
                    }
                    Err(error) => tracing::warn!(%error, "reading the instance stats failed"),
                }
                if std::mem::take(&mut this.stats.again) {
                    // Force a fresh read of the current list.
                    this.stats.generation = u64::MAX;
                    this.sync_stats(cx);
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn render_header(&self, instance: &Instance, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let config = &instance.config;
        let sep = || div().text_color(theme.border).child("|");
        let loader = loader_color(config, cx);
        let java = self
            .java(&instance.id)
            .map(|v| t!("instances.java", version = v).to_string());
        let played = match config.last_played {
            Some(t) => t!("instances.played", when = time_ago(t, now_secs())).to_string(),
            None => t!("instances.played_never").to_string(),
        };
        let teal = ProductColors::get(cx).signature();
        let launch = ButtonCustomVariant::new(cx)
            .color(teal)
            .foreground(theme.primary_foreground)
            .hover(teal.opacity(0.9))
            .active(teal.opacity(0.8));
        h_flex()
            .flex_none()
            .px(GUTTER)
            .pt(rems(1.125))
            .pb(rems(1.))
            .gap(rems(0.875))
            .child(instance_icon(&icon_for(instance), HEADER_ICON, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(rems(0.375))
                    .child(
                        div()
                            .id("instance-name")
                            .text_size(HEADER_TITLE)
                            .line_height(rems(1.75))
                            .strong(FontWeight::SEMIBOLD, cx)
                            .text_color(theme.foreground)
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(config.name.clone()),
                    )
                    .child(
                        h_flex()
                            .id("instance-meta")
                            .gap_2()
                            .text_size(TEXT_SECONDARY)
                            .text_color(theme.muted_foreground)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_none()
                                    .px_1p5()
                                    .rounded_sm()
                                    .bg(loader.opacity(0.14))
                                    .text_color(loader)
                                    .line_height(rems(1.125))
                                    .child(loader_full(config)),
                            )
                            .child(config.mc_version().to_string())
                            .when_some(java, |d, java| d.child(sep()).child(java))
                            .child(sep())
                            .child(played),
                    ),
            )
            .map(|d| {
                let model = self.model.read(cx);
                match (model.op(&instance.id), model.run_state(&instance.id)) {
                    (Some(op), _) => d.child(self.render_op(&instance.id, op, cx)),
                    (None, Some(RunState::Exited(exit))) => {
                        d.child(self.render_crash(&instance.id, exit.saved_log.clone(), cx))
                    }
                    (None, Some(RunState::HeldElsewhere { .. })) => d.child(
                        div()
                            .id("instance-elsewhere")
                            .test_support()
                            .min_w_0()
                            .text_size(TEXT_SECONDARY)
                            .text_color(theme.muted_foreground)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(t!("launch.elsewhere").to_string()),
                    ),
                    _ => d,
                }
            })
            .map(|d| {
                let model = self.model.read(cx);
                if !model.is_running(&instance.id) {
                    return d;
                }
                let stopping = model.is_stopping(&instance.id);
                let (model, id) = (self.model.clone(), instance.id.clone());
                d.child(
                    Button::new("instance-stop")
                        .danger()
                        .outline()
                        .icon(Icon::new(IconName::Square))
                        .label(t!("launch.force_stop").to_string())
                        .loading(stopping)
                        .disabled(stopping)
                        .h(rems(2.25))
                        .on_click(move |_, window, cx| {
                            stop_dialog::open(model.clone(), id.clone(), window, cx);
                        }),
                )
            })
            .child({
                let model = self.model.read(cx);
                let busy = model.is_busy(&instance.id);
                let starting = matches!(model.run_state(&instance.id), Some(RunState::Starting));
                let (model, id) = (self.model.clone(), instance.id.clone());
                Button::new("launch")
                    .custom(launch)
                    // A custom variant's resting fill is drawn at 20 %
                    // opacity by gpui-component; set it solid.
                    .bg(teal)
                    .icon(IconName::Play)
                    .label(t!("instances.launch").to_string())
                    .loading(starting)
                    .disabled(busy)
                    .h(rems(2.25))
                    .px(rems(1.25))
                    .gap_1p5()
                    .text_size(rems(0.875))
                    .strong(FontWeight::SEMIBOLD, cx)
                    .on_click(move |_, _, cx| {
                        model.update(cx, |m, cx| m.launch(&id, cx));
                    })
            })
    }

    /// "游戏异常退出，日志已保存" with "打开文件夹" (the saved log) and
    /// dismiss.
    fn render_crash(
        &self,
        id: &str,
        saved_log: Option<std::path::PathBuf>,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let message = match saved_log {
            Some(_) => t!("launch.crashed_saved"),
            None => t!("launch.crashed"),
        }
        .to_string();
        let open = saved_log.map(|path| {
            Button::new("instance-crash-open")
                .small()
                .icon(Icon::new(IconName::FolderOpen))
                .label(t!("common.open_folder").to_string())
                .on_click(move |_, _, cx| cx.reveal_path(&path))
        });
        let (model, key) = (self.model.clone(), id.to_string());
        let dismiss = Button::new("instance-crash-dismiss")
            .ghost()
            .small()
            .label(t!("ops.dismiss").to_string())
            .on_click(move |_, _, cx| {
                model.update(cx, |m, cx| m.dismiss_exit(&key, cx));
            });
        v_flex()
            .id("instance-crash")
            .test_support()
            .min_w_0()
            .w(rems(22.))
            .gap(rems(0.375))
            .text_size(TEXT_SECONDARY)
            .child(
                div()
                    .id("instance-crash-text")
                    .aria_label(message.clone())
                    .test_support()
                    .text_color(theme.danger)
                    .line_clamp(2)
                    .child(message),
            )
            .child(h_flex().gap_1().children(open).child(dismiss))
    }

    /// The running operation's bar with "step · amount" below it, or the
    /// failed one's message with retry and dismiss.
    fn render_op(&self, id: &str, op: &OpState, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let area = v_flex()
            .id("instance-op")
            // Gives way to the title column when the window is narrow.
            .min_w_0()
            .w(rems(16.))
            .gap(rems(0.375))
            .text_size(TEXT_SECONDARY);
        match op {
            OpState::Running { progress, .. } => {
                let fraction = progress.as_ref().and_then(|p| p.fraction);
                let step = progress.as_ref().map(|p| p.step.clone());
                let amount = progress.as_ref().and_then(|p| p.amount.clone());
                let text = [step, amount].into_iter().flatten().collect::<Vec<_>>();
                area.child(
                    Progress::new("instance-op-bar")
                        .value(fraction.unwrap_or(0.) * 100.)
                        .loading(fraction.is_none()),
                )
                .child(
                    div()
                        .id("instance-op-text")
                        .aria_label(text.join(" · "))
                        .test_support()
                        .text_color(theme.muted_foreground)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(text.join(" · ")),
                )
            }
            OpState::Failed {
                message,
                retry,
                fix,
                ..
            } => {
                let (model, key) = (self.model.clone(), id.to_string());
                let dismiss = {
                    let (model, key) = (model.clone(), key.clone());
                    Button::new("instance-op-dismiss")
                        .ghost()
                        .small()
                        .label(t!("ops.dismiss").to_string())
                        .on_click(move |_, _, cx| {
                            model.update(cx, |m, cx| m.dismiss_op(&key, cx));
                        })
                };
                // A fix takes the place of retrying.
                let action = match fix {
                    Some(fix) => Some(
                        Button::new("instance-op-fix")
                            .small()
                            .label(fix.label())
                            .on_click(move |_, _, cx| {
                                model.update(cx, |m, cx| m.apply_fix(&key, cx));
                            }),
                    ),
                    None => retry.is_some().then(|| {
                        Button::new("instance-op-retry")
                            .small()
                            .label(t!("ops.retry").to_string())
                            .on_click(move |_, _, cx| {
                                model.update(cx, |m, cx| m.retry_op(&key, cx));
                            })
                    }),
                };
                area.w(rems(22.))
                    .child(
                        div()
                            .id("instance-op-error")
                            .aria_label(message.clone())
                            .test_support()
                            .text_color(theme.danger)
                            .line_clamp(2)
                            .child(message.clone()),
                    )
                    .child(h_flex().gap_1().children(action).child(dismiss))
            }
        }
    }

    fn render_tabs(&self, instance: &Instance, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let teal = ProductColors::get(cx).signature();
        let stats = self.stats(&instance.id);
        let this = cx.weak_entity();
        let count = DetailTab::ALL.len();
        Tabs::new("instance-tabs")
            .flex()
            .flex_none()
            .items_center()
            .h(TABS_H)
            .px(GUTTER)
            .gap_5()
            .border_b_1()
            .border_color(border)
            .children(DetailTab::ALL.into_iter().enumerate().map(|(i, tab)| {
                let selected = tab == self.tab;
                let badge = match tab {
                    DetailTab::Mods => stats.and_then(|s| s.mods).filter(|n| *n > 0),
                    DetailTab::Worlds => stats.and_then(|s| s.worlds).filter(|n| *n > 0),
                    _ => None,
                };
                let this = this.clone();
                TabElement::new(tab.id())
                    .selected(selected)
                    .set_position(i + 1, count)
                    .accessibility_label(tab.label())
                    .h_full()
                    .px_0p5()
                    .gap_1()
                    .relative()
                    .whitespace_nowrap()
                    .text_size(TEXT_PRIMARY)
                    .text_color(if selected { fg } else { muted })
                    .when(selected, |t| t.strong(FontWeight::MEDIUM, cx))
                    .when(!selected, |t| t.hover(move |s| s.text_color(fg)))
                    .child(tab.label())
                    .when_some(badge, |t, n| {
                        t.child(
                            div()
                                .text_size(TEXT_SECONDARY)
                                .font_weight(FontWeight::NORMAL)
                                .text_color(muted)
                                .child(n.to_string()),
                        )
                    })
                    .when(selected, |t| {
                        t.child(
                            div()
                                .absolute()
                                // Over the strip's 1px bottom divider.
                                .bottom(px(-1.))
                                .left_0()
                                .right_0()
                                .h(rems(0.125))
                                .bg(teal),
                        )
                    })
                    .on_click(move |_, _, cx| {
                        this.update(cx, |v, cx| v.set_tab(tab, cx)).ok();
                    })
            }))
    }

    fn render_body(&self, instance: &Instance, cx: &App) -> AnyElement {
        match self.tab {
            DetailTab::Log => {
                let elsewhere = matches!(
                    self.model.read(cx).run_state(&instance.id),
                    Some(RunState::HeldElsewhere { .. })
                );
                // The other process shows this game's log; this one does
                // not tail it (the folder button opens current.log).
                let note = elsewhere.then(|| {
                    let theme = cx.theme();
                    div()
                        .id("log-elsewhere")
                        .test_support()
                        .flex_none()
                        .px(GUTTER)
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(TEXT_SECONDARY)
                        .text_color(theme.muted_foreground)
                        .child(t!("launch.elsewhere_log").to_string())
                });
                v_flex()
                    .size_full()
                    .children(note)
                    .child(div().flex_1().min_h_0().children(self.log.clone()))
                    .into_any_element()
            }
            DetailTab::Versions => div()
                .size_full()
                .children(self.versions.clone())
                .into_any_element(),
            DetailTab::Overview => overview::render(instance, self.stats(&instance.id), cx),
            tab => placeholder::page(tab.icon(), tab.label()).into_any_element(),
        }
    }
}

impl Render for DetailView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = cx.theme().background;
        let model = self.model.read(cx);
        let Some(Entry::Healthy(instance)) = model.selected_entry() else {
            return div().id("instance-detail").into_any_element();
        };
        let instance = instance.clone();
        v_flex()
            .id("instance-detail")
            .test_support()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .size_full()
            .bg(bg)
            .child(self.render_header(&instance, cx))
            .child(self.render_tabs(&instance, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_body(&instance, cx)),
            )
            .into_any_element()
    }
}
