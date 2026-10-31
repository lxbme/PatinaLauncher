//! The settings page: the launcher's global settings, one scrolling column
//! of sections with an index on the left (collapsed when the window is
//! narrow; the column then scrolls on its own):
//!
//! - 外观 ([`appearance`]): the interface scale (the steps that fit the
//!   display, in step with `Ctrl +` / `Ctrl −`), the language (switches at
//!   once) and the theme (dark only for now);
//! - 下载 ([`download`]): the preferred download source and "立即测速";
//! - Java 与内存 ([`java`]): the defaults for memory, the GC preset and the
//!   JVM arguments (the instance settings' rules), the managed runtimes
//!   (deletable, after asking) and the Java found on this computer;
//! - 游戏启动后 and 数据目录 ([`behavior`]);
//! - 关于 ([`about`]).
//!
//! The app's own settings (`gui.toml`: scale, language, after launch) go
//! through [`SettingsStore`](crate::settings::SettingsStore); the launcher's
//! (`config.toml`) through [`Core::update_config`], one save at a time, in
//! order. When `config.toml` could not be read the page says so above the
//! sections; the first save then backs the file up first and says where.
//! Text fields save when they lose focus or on Enter; a value that does not
//! validate shows its reason under the field and is not saved.

mod about;
mod appearance;
mod behavior;
mod download;
mod java;

pub use about::{LICENSE, REPO_URL};
pub use appearance::scale_step_id;
pub use behavior::AFTER_LAUNCH;
pub use download::{SOURCES, speed_row_id};
pub use java::{runtime_delete_id, runtime_row_id};

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Selectable, Sizable, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Rems, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Window, div, rems,
};
use patina_core::config::{ConfigError, GlobalConfig};
use rust_i18n::t;

use crate::core_bridge::{
    ConfigSaveError, ConfigSaved, Core, LoadError, Revision, SpeedTestResult, config_reason,
};
use crate::fonts::Strong;
use crate::instances::{InstancesModel, JavaList, detect_java, java_list};
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY, scroll_y};

/// Widest a section grows.
const CONTENT_W: Rems = Rems(40.);
/// Inset of the sections from the column's edges.
const PAGE_PAD: Rems = Rems(1.5);
/// Width of the section index.
const INDEX_W: Rems = Rems(10.);
/// Narrowest window (in rems, so it holds at every scale) that shows the
/// index: below it the sections take the whole width.
const INDEX_MIN_WINDOW: Rems = Rems(64.);

/// What to say under a field whose save failed: the reason once, as a
/// sentence of its own when it is complete (a refused backup), else after
/// "无法保存设置：".
fn save_failed(error: &ConfigSaveError) -> String {
    match error {
        ConfigSaveError::Backup(_) => error.to_string(),
        _ => t!("prefs.save_failed", error = error).to_string(),
    }
}

/// A section of the page, in the order shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Appearance,
    Download,
    Java,
    AfterLaunch,
    Data,
    About,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Section::Appearance,
        Section::Download,
        Section::Java,
        Section::AfterLaunch,
        Section::Data,
        Section::About,
    ];

    /// The section's heading, also its entry in the index.
    pub fn title(self) -> String {
        match self {
            Section::Appearance => t!("prefs.appearance"),
            Section::Download => t!("prefs.download"),
            Section::Java => t!("prefs.java"),
            Section::AfterLaunch => t!("prefs.after_launch"),
            Section::Data => t!("prefs.data"),
            Section::About => t!("prefs.about"),
        }
        .to_string()
    }

    /// Element id of the section.
    pub fn id(self) -> &'static str {
        match self {
            Section::Appearance => "prefs-appearance",
            Section::Download => "prefs-download",
            Section::Java => "prefs-java",
            Section::AfterLaunch => "prefs-after-launch",
            Section::Data => "prefs-data",
            Section::About => "prefs-about",
        }
    }

    /// Element id of the section's entry in the index.
    pub fn index_id(self) -> &'static str {
        match self {
            Section::Appearance => "prefs-index-appearance",
            Section::Download => "prefs-index-download",
            Section::Java => "prefs-index-java",
            Section::AfterLaunch => "prefs-index-after-launch",
            Section::Data => "prefs-index-data",
            Section::About => "prefs-index-about",
        }
    }

    fn position(self) -> usize {
        Section::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// A setting saved to `config.toml`, for showing a failed save beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Source,
    Memory,
    Gc,
    Args,
}

/// The reason under each field, if any.
#[derive(Default)]
struct Errors {
    source: Option<SharedString>,
    memory: Option<SharedString>,
    gc: Option<SharedString>,
    args: Option<SharedString>,
}

impl Errors {
    fn slot(&mut self, field: Field) -> &mut Option<SharedString> {
        match field {
            Field::Source => &mut self.source,
            Field::Memory => &mut self.memory,
            Field::Gc => &mut self.gc,
            Field::Args => &mut self.args,
        }
    }
}

/// A change of `config.toml` waiting to be saved.
type ConfigEdit = Box<dyn FnOnce(&mut GlobalConfig) + Send>;

/// Opens a folder in the system file manager (injected by tests).
type Opener = Rc<dyn Fn(&Path, &mut App)>;

/// Opens a web address in the browser (injected by tests).
type UrlOpener = Rc<dyn Fn(&str, &mut App)>;

/// The download-source speed test on show.
#[derive(Default)]
struct Speed {
    running: bool,
    result: Option<SpeedTestResult>,
    error: Option<SharedString>,
    task: Option<Task<()>>,
}

/// The settings page (an entity). See the module docs.
pub struct SettingsPage {
    model: Entity<InstancesModel>,
    /// `config.toml` as last read; `None` until the first read lands.
    config: Option<GlobalConfig>,
    load_error: Option<LoadError<ConfigError>>,
    config_rev: Revision,
    config_task: Task<()>,
    /// Saves waiting for the one in flight, run in order.
    queue: VecDeque<(Field, ConfigEdit)>,
    saving: bool,
    idle_waiters: Vec<futures::channel::oneshot::Sender<()>>,
    /// Latest accepted intent, kept until matching readback or its final write fails.
    requested_memory: Option<(u32, Option<u32>)>,
    requested_args: Option<String>,
    requested_gc: Option<patina_core::jvm::GcPreset>,
    requested_source: Option<patina_core::config::SourceChoice>,
    errors: Errors,
    /// The config changed since the inputs were last set from it.
    inputs_stale: bool,
    /// A never-shown page has blank inputs, not user edits to flush.
    inputs_ready: bool,
    speed: Speed,
    min_mem: Entity<InputState>,
    max_mem: Entity<InputState>,
    args: Entity<TextareaState>,
    java_list: Entity<JavaList>,
    /// The installed managed runtimes (component, version), once listed.
    runtimes: Option<Vec<(String, String)>>,
    runtimes_rev: Revision,
    runtimes_task: Task<()>,
    /// Runtimes being deleted.
    removing: HashSet<String>,
    scroll: ScrollHandle,
    opener: Opener,
    url_opener: UrlOpener,
    /// The repository link's place in the Tab order.
    repo_focus: gpui_kit::FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    /// The page; `model` tells whether games or tasks are under way (a
    /// runtime is not deleted then).
    pub fn new(model: Entity<InstancesModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let min_mem = cx.new(|cx| InputState::new(window, cx));
        let max_mem = cx.new(|cx| InputState::new(window, cx));
        let auto = "instance_settings.max_auto_placeholder";
        crate::i18n::localized_placeholder(&max_mem, auto, window, cx);
        let args = cx.new(|cx| TextareaState::new(window, cx));
        let java_list = java_list(cx);
        let commit = |commit: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            move |this: &mut Self,
                  _: &Entity<InputState>,
                  event: &InputEvent,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    commit(this, window, cx);
                }
            }
        };
        let _subscriptions = vec![
            cx.subscribe_in(&min_mem, window, commit(Self::commit_memory)),
            cx.subscribe_in(&max_mem, window, commit(Self::commit_memory)),
            cx.subscribe_in(
                &args,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => this.args_changed(cx),
                    // Enter is a new line here.
                    InputEvent::Blur => this.commit_args(window, cx),
                    _ => {}
                },
            ),
            cx.observe(&java_list, |_, _, cx| cx.notify()),
            // Whether a runtime may be deleted: only that changes here.
            {
                let activity = model.read(cx).activity().clone();
                crate::instances::observe_activity(&activity, cx, |a| {
                    a.has_work_in_flight() || a.any_game_busy()
                })
            },
        ];
        let mut this = Self {
            model,
            config: None,
            load_error: None,
            config_rev: Revision::default(),
            config_task: Task::ready(()),
            queue: VecDeque::new(),
            saving: false,
            idle_waiters: Vec::new(),
            requested_memory: None,
            requested_args: None,
            requested_gc: None,
            requested_source: None,
            errors: Errors::default(),
            inputs_stale: false,
            inputs_ready: false,
            speed: Speed::default(),
            min_mem,
            max_mem,
            args,
            java_list,
            runtimes: None,
            runtimes_rev: Revision::default(),
            runtimes_task: Task::ready(()),
            removing: HashSet::new(),
            scroll: ScrollHandle::new(),
            opener: Rc::new(|dir, cx| cx.open_with_system(dir)),
            url_opener: Rc::new(|url, cx| cx.open_url(url)),
            repo_focus: cx.focus_handle().tab_stop(true),
            _subscriptions,
        };
        this.reload_config(cx);
        this
    }

    /// The page came on show: reads the config, the runtimes and the last
    /// speed test again, and looks for Java unless that was done.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        self.reload_config(cx);
        self.reload_runtimes(cx);
        if !self.speed.running
            && let Some(last) = Core::global(cx).last_speed_test()
        {
            self.speed.result = Some(last);
        }
        detect_java(&self.java_list, cx);
        cx.notify();
    }

    /// Replaces how "打开" opens the data folder (tests; the headless
    /// platform cannot open anything).
    pub fn set_opener(&mut self, opener: impl Fn(&Path, &mut App) + 'static) {
        self.opener = Rc::new(opener);
    }

    /// Replaces how the repository link opens its address (tests).
    pub fn set_url_opener(&mut self, opener: impl Fn(&str, &mut App) + 'static) {
        self.url_opener = Rc::new(opener);
    }

    /// Whether a managed runtime is being deleted (quitting would cut the
    /// deletion short).
    pub fn is_removing_runtime(&self) -> bool {
        !self.removing.is_empty()
    }

    /// The JVM arguments field.
    pub fn args_input(&self) -> &Entity<TextareaState> {
        &self.args
    }

    /// `config.toml` as last read.
    pub fn config(&self) -> Option<&GlobalConfig> {
        self.config.as_ref()
    }

    /// The installed managed runtimes (component, version), once listed.
    pub fn runtimes(&self) -> Option<&[(String, String)]> {
        self.runtimes.as_deref()
    }

    /// Whether a speed test is running.
    pub fn is_speed_testing(&self) -> bool {
        self.speed.running
    }

    /// Whether a save is queued or in flight.
    pub fn is_saving(&self) -> bool {
        self.saving || !self.queue.is_empty()
    }

    /// Validates focused inputs and waits for all queued writes to finish.
    pub fn flush(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl std::future::Future<Output = ()> + use<> {
        if self.inputs_ready {
            self.commit_memory(window, cx);
            self.commit_args(window, cx);
        }
        let (done, wait) = futures::channel::oneshot::channel();
        if self.is_saving() {
            self.idle_waiters.push(done);
        } else {
            done.send(()).ok();
        }
        async move {
            wait.await.ok();
        }
    }

    /// Shows `section` at the top of the column.
    pub fn scroll_to(&mut self, section: Section, cx: &mut Context<Self>) {
        self.scroll.scroll_to_top_of_item(section.position());
        cx.notify();
    }

    // --- reading ------------------------------------------------------------

    /// Reads the config and whether it failed to load; a newer read
    /// supersedes this one.
    fn reload_config(&mut self, cx: &mut Context<Self>) {
        let id = self.config_rev.start();
        let config = Core::read_config(cx);
        let error = Core::config_load_error(cx);
        self.config_task = cx.spawn(async move |this, cx| {
            let (config, error) = (config.await, error.await);
            this.update(cx, |this, cx| {
                if !this.config_rev.is_current(id) {
                    return;
                }
                if let Ok(config) = config {
                    if !this.is_saving() {
                        if this.requested_memory
                            == Some((config.java.min_mem_mib, config.java.max_mem_mib))
                        {
                            this.requested_memory = None;
                        }
                        if this.requested_args.as_ref() == Some(&config.java.extra_jvm_args) {
                            this.requested_args = None;
                        }
                        if this.requested_gc == Some(config.java.gc_preset) {
                            this.requested_gc = None;
                        }
                        if this.requested_source == Some(config.download.source) {
                            this.requested_source = None;
                        }
                    }
                    if this.config.as_ref() != Some(&config) {
                        this.config = Some(config);
                        this.inputs_stale = true;
                    }
                }
                if let Ok(error) = error {
                    this.load_error = error;
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn reload_runtimes(&mut self, cx: &mut Context<Self>) {
        let id = self.runtimes_rev.start();
        let root = Core::global(cx).data.runtimes();
        let list = crate::core_bridge::run_blocking(cx, move || {
            patina_core::java::ManagedRuntimes::new(root).list_installed()
        });
        self.runtimes_task = cx.spawn(async move |this, cx| {
            let list = list.await;
            this.update(cx, |this, cx| {
                if this.runtimes_rev.is_current(id) {
                    // A failed read keeps what was shown.
                    if let Ok(list) = list {
                        this.runtimes = Some(list);
                    }
                    cx.notify();
                }
            })
            .ok();
        });
    }

    // --- saving -------------------------------------------------------------

    /// Saves `edit` of `config.toml` once the saves before it are done.
    fn edit(
        &mut self,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut GlobalConfig) + Send + 'static,
    ) {
        self.queue.push_back((field, Box::new(edit)));
        self.pump(window, cx);
        cx.notify();
    }

    /// Starts the next queued save unless one is in flight: saves run one
    /// at a time, so they reach the disk in the order they were made.
    fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some((field, edit)) = self.queue.pop_front() else {
            for waiter in self.idle_waiters.drain(..) {
                waiter.send(()).ok();
            }
            return;
        };
        self.saving = true;
        let save = Core::update_config(cx, edit);
        cx.spawn_in(window, async move |this, cx| {
            let result = save.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                let result = match result {
                    Ok(saved) => saved.map_err(|e| save_failed(&e)),
                    Err(e) => Err(t!("prefs.save_failed", error = e).to_string()),
                };
                *this.errors.slot(field) = match &result {
                    Ok(_) => None,
                    Err(error) => {
                        if !this.queue.iter().any(|(queued, _)| *queued == field) {
                            match field {
                                Field::Memory => this.requested_memory = None,
                                Field::Args => this.requested_args = None,
                                Field::Gc => this.requested_gc = None,
                                Field::Source => this.requested_source = None,
                            }
                        }
                        tracing::warn!(%error, ?field, "saving a setting failed");
                        Some(error.clone().into())
                    }
                };
                if let Ok(saved) = result {
                    Self::saved(&saved, window, cx);
                }
                // What is saved now, and the fields put back after a
                // failure.
                this.inputs_stale = true;
                this.reload_config(cx);
                this.pump(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Tells where the unreadable `config.toml` was kept, after the save
    /// that backed it up.
    fn saved(saved: &ConfigSaved, window: &mut Window, cx: &mut App) {
        if let Some(backup) = &saved.backup {
            let text = t!("prefs.config_backed_up", path = backup.display()).to_string();
            window.push_notification(Notification::info(text), cx);
        }
    }

    // --- drawing helpers ------------------------------------------------------

    /// A section: its heading, then its fields, closed by a hairline.
    fn section(section: Section, cx: &App) -> impl ParentElement + Styled + IntoElement + use<> {
        v_flex()
            .id(section.id())
            .test_support()
            .w_full()
            .max_w(CONTENT_W)
            .gap_3()
            .py_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_size(TEXT_ROW)
                    .strong(FontWeight::SEMIBOLD, cx)
                    .text_color(cx.theme().foreground)
                    .child(section.title()),
            )
    }

    /// A field: its label above what follows.
    fn field(label: String, cx: &App) -> gpui_kit::Div {
        v_flex().gap_1().child(
            div()
                .text_size(TEXT_SECONDARY)
                .strong(FontWeight::MEDIUM, cx)
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
    }

    /// A muted line under a field.
    fn hint(id: impl Into<ElementId>, text: impl Into<SharedString>, cx: &App) -> AnyElement {
        let text = text.into();
        div()
            .id(id.into())
            .test_support()
            .aria_label(text.clone())
            .text_size(TEXT_SECONDARY)
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .into_any_element()
    }

    /// The reason under a field, in the danger colour.
    fn error(id: &'static str, text: Option<&SharedString>, cx: &App) -> Option<AnyElement> {
        text.map(|text| {
            div()
                .id(id)
                .test_support()
                .aria_label(text.clone())
                .text_size(TEXT_SECONDARY)
                .text_color(cx.theme().danger)
                .child(text.clone())
                .into_any_element()
        })
    }

    /// `config.toml` is not what is shown: why, above the sections.
    fn render_load_error(&self, cx: &App) -> Option<impl IntoElement + use<>> {
        let text = match self.load_error.as_ref()? {
            LoadError::Failed(error) => t!("prefs.config_failed", error = config_reason(error)),
            LoadError::NotLoaded => t!("prefs.config_not_loaded"),
        };
        let text = SharedString::from(text.to_string());
        let theme = cx.theme();
        Some(
            h_flex()
                .flex_none()
                .gap_2()
                .px(GUTTER)
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size(rems(1.))
                        .text_color(theme.warning),
                )
                .child(
                    div()
                        .id("prefs-config-error")
                        .test_support()
                        .aria_label(text.clone())
                        .min_w_0()
                        .text_size(TEXT_SECONDARY)
                        .text_color(theme.foreground)
                        .child(text),
                ),
        )
    }

    /// The index: one entry per section; the one at the top of the column
    /// is marked.
    fn render_index(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let current = Section::ALL
            .get(self.scroll.top_item())
            .copied()
            .unwrap_or(Section::Appearance);
        v_flex()
            .id("prefs-index")
            .test_support()
            .aria_label(t!("prefs.sections").to_string())
            .flex_none()
            .w(INDEX_W)
            .h_full()
            .gap_0p5()
            .p_2()
            .border_r_1()
            .border_color(cx.theme().border)
            .children(Section::ALL.map(|section| {
                Button::new(section.index_id())
                    .ghost()
                    .small()
                    .w_full()
                    .justify_start()
                    .selected(section == current)
                    .label(section.title())
                    .on_click(cx.listener(move |this, _, _, cx| this.scroll_to(section, cx)))
            }))
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.inputs_stale) {
            cx.defer_in(window, |this, window, cx| this.apply_inputs(window, cx));
        }
        let wide = window.viewport_size().width >= INDEX_MIN_WINDOW.to_pixels(window.rem_size());
        let sections: Vec<AnyElement> = vec![
            self.render_appearance(cx).into_any_element(),
            self.render_download(cx).into_any_element(),
            self.render_java(cx).into_any_element(),
            self.render_after_launch(cx).into_any_element(),
            self.render_data(cx).into_any_element(),
            self.render_about(window, cx).into_any_element(),
        ];
        let content = v_flex()
            .id("prefs-content")
            .test_support()
            .px(PAGE_PAD)
            .text_size(TEXT_PRIMARY)
            .children(sections);
        v_flex()
            .id("prefs-page")
            .test_support()
            .size_full()
            .children(self.render_load_error(cx))
            .child(
                h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .when(wide, |row| row.child(self.render_index(cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(scroll_y(content, &self.scroll)),
                    ),
            )
    }
}

/// `ALL` and `position` agree.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_errors_say_their_reason_once() {
        let _locale = crate::i18n::locale_lock();
        crate::i18n::set_language(crate::i18n::Language::En);
        let io = || ConfigError::Io("Permission denied".into());
        assert_eq!(
            save_failed(&ConfigSaveError::Save(io())),
            "Couldn’t save the settings: Permission denied"
        );
        assert_eq!(
            save_failed(&ConfigSaveError::Backup("Disk full".into())),
            "Couldn’t back up the original config.toml, so nothing was saved: Disk full"
        );
        assert_eq!(config_reason(&ConfigError::Parse("bad".into())), "bad");
    }

    #[test]
    fn sections_are_in_order() {
        for (ix, section) in Section::ALL.iter().enumerate() {
            assert_eq!(section.position(), ix);
        }
    }
}
