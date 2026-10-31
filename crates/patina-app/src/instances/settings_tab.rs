//! The "设置" tab: one form per selected instance ([`SettingsForm`]).
//!
//! - 基本: the name (saved on blur or Enter) and the icon ([`IconPicker`]).
//! - Java 与内存: "使用全局设置" on (no override) or off (an override seeded
//!   from the global config), then Java (managed, detected, or a chosen
//!   file), minimum and maximum memory, the GC preset and the JVM arguments.
//! - 游戏窗口: the game's default or a custom size.
//! - NeoForge (only for that loader): the early loading window switch.
//! - 危险操作: deleting the instance, behind a confirmation.
//!
//! A text field saves when it loses focus (the name and the numbers also on
//! Enter). A value that does not validate shows its reason under the field
//! and is not saved; the text stays so it can be fixed. A save that fails in
//! the core shows the core's error and puts the field back to the saved
//! value. Every save goes through [`run_blocking`] and refreshes the model.
//! Renaming and deleting are disabled, with the reason, while the instance is
//! busy.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};

use futures::channel::oneshot;
use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    App, AppContext, Context, Entity, Focusable, FontWeight, Global, InteractiveElement,
    IntoElement, ParentElement, PathPromptOptions, Rems, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Window, div, rems,
};
use patina_core::config::JavaSettings;
use patina_core::instance::{
    Instance, InstanceConfig, InstanceError, InstanceIcon, InstanceStore, WindowSize,
};
use patina_core::java::{JavaInstall, detect_all, probe};
use patina_core::jvm::{GcPreset, validate_extra_jvm_args};
use patina_core::loader::LoaderKind;
use rust_i18n::t;

use super::icon::IconChoice;
use super::icon_picker::{IconPicked, IconPicker};
use super::model::{Entry, InstancesChanged, InstancesModel};
use super::{icon_for, instance_icon, overview};
use crate::core_bridge::{Core, Revision, run_blocking};
use crate::fonts::Strong;
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY, scroll_y};

/// Widest the form grows.
const CONTENT_W: Rems = Rems(40.);
/// Width of the numeric inputs.
const NUMBER_W: Rems = Rems(9.);
/// What "自定义" starts from: Minecraft's own default window.
const DEFAULT_WINDOW: (u32, u32) = (854, 480);
/// The GC presets in the order they are listed.
/// Largest memory value accepted, in MiB (1 TiB): far beyond any machine
/// today, but it keeps the value inside what the JVM flags can express.
pub const MAX_MEM_MIB: u32 = 1_048_576;
const GC_PRESETS: [GcPreset; 4] = [
    GcPreset::None,
    GcPreset::G1Aikar,
    GcPreset::GenerationalZgc,
    GcPreset::Custom,
];

// --- the detected Java installs, shared by the session ---------------------

/// The Java installs found on this machine: looked for once per session, in
/// the background, when a form first shows the Java choice.
pub struct JavaList {
    installs: Option<Vec<JavaInstall>>,
    loading: bool,
}

struct JavaListGlobal(Entity<JavaList>);

impl Global for JavaListGlobal {}

fn java_list(cx: &mut App) -> Entity<JavaList> {
    if let Some(global) = cx.try_global::<JavaListGlobal>() {
        return global.0.clone();
    }
    let list = cx.new(|_| JavaList {
        installs: None,
        loading: false,
    });
    cx.set_global(JavaListGlobal(list.clone()));
    list
}

/// Makes `installs` the detected Java installs, so no detection runs (tests).
pub fn seed_java_list(installs: Vec<JavaInstall>, cx: &mut App) {
    let list = java_list(cx);
    list.update(cx, |list, cx| {
        list.installs = Some(installs);
        list.loading = false;
        cx.notify();
    });
}

/// Starts looking for Java installs unless that was done or is under way.
fn detect_java(list: &Entity<JavaList>, cx: &mut App) {
    let state = list.read(cx);
    if state.installs.is_some() || state.loading {
        return;
    }
    list.update(cx, |list, _| list.loading = true);
    let job = run_blocking(cx, detect_all);
    let list = list.clone();
    cx.spawn(async move |cx| {
        let found = job.await.unwrap_or_default();
        list.update(cx, |list, cx| {
            list.loading = false;
            list.installs = Some(found);
            cx.notify();
        });
    })
    .detach();
}

// --- the form ---------------------------------------------------------------

/// A setting that saves, for putting a field back after a failed save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Java,
    Memory,
    Gc,
    Args,
    Window,
    Global,
}

/// The error under each field, if any.
#[derive(Default)]
struct Errors {
    name: Option<SharedString>,
    java: Option<SharedString>,
    memory: Option<SharedString>,
    args: Option<SharedString>,
    window: Option<SharedString>,
    neoforge: Option<SharedString>,
}

impl Errors {
    fn slot(&mut self, field: Field) -> &mut Option<SharedString> {
        match field {
            Field::Name => &mut self.name,
            Field::Java | Field::Gc | Field::Global => &mut self.java,
            Field::Memory => &mut self.memory,
            Field::Args => &mut self.args,
            Field::Window => &mut self.window,
        }
    }
}

/// Where the NeoForge early-window switch stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Early {
    Loading,
    /// Whether the early window is disabled.
    Value(bool),
    /// `fml.toml` could not be read; the switch is unavailable.
    Failed,
}

/// A save: a core call on the store and the instance id.
type SaveJob = Box<dyn FnOnce(&InstanceStore, &str) -> Result<(), InstanceError> + Send>;

/// Why the GC options that need a known Java are unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JavaUnknown {
    /// Still being read.
    Checking,
    /// Could not be determined.
    Unknown,
}

/// The settings form of one instance (an entity). See the module docs.
pub struct SettingsForm {
    model: Entity<InstancesModel>,
    id: String,
    name: Entity<InputState>,
    min_mem: Entity<InputState>,
    max_mem: Entity<InputState>,
    args: Entity<TextareaState>,
    width: Entity<InputState>,
    height: Entity<InputState>,
    icon_picker: Entity<IconPicker>,
    icon_open: bool,
    java_list: Entity<JavaList>,
    /// Java majors found by probing a chosen or saved path that no detected
    /// install covers.
    probed: HashMap<PathBuf, Option<u32>>,
    /// Paths a probe was started for.
    probe_started: HashSet<PathBuf>,
    probing: Vec<Task<()>>,
    /// Saves waiting for the one in flight, run in order.
    queue: VecDeque<(Field, SaveJob)>,
    saving: bool,
    /// Resolved once no save is queued or in flight.
    idle_waiters: Vec<oneshot::Sender<()>>,
    /// The override "使用全局设置" turned off, kept for turning it on again.
    discarded: Arc<Mutex<Option<JavaSettings>>>,
    /// The Java and GC preset a reset notice was already given for.
    gc_reset_for: Option<(Option<PathBuf>, GcPreset)>,
    required_done: bool,
    /// The model or the Java list changed since `render` last followed up.
    stale: bool,
    errors: Errors,
    /// The form's scroll.
    scroll: ScrollHandle,
    /// The config the inputs were last set from.
    seen: Option<InstanceConfig>,
    /// The Java the instance's Minecraft version needs, once read.
    required: Option<u32>,
    required_mc: Option<String>,
    required_revision: Revision,
    required_task: Task<()>,
    early: Early,
    early_dir: Option<PathBuf>,
    early_revision: Revision,
    early_task: Task<()>,
    deleting: bool,
    _subscriptions: Vec<Subscription>,
}

impl SettingsForm {
    pub fn new(
        model: Entity<InstancesModel>,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let number = |window: &mut Window, cx: &mut Context<Self>, hint: String| {
            cx.new(|cx| InputState::new(window, cx).placeholder(hint))
        };
        let name = cx.new(|cx| InputState::new(window, cx));
        let min_mem = number(window, cx, String::new());
        let max_mem = number(
            window,
            cx,
            t!("instance_settings.max_auto_placeholder").to_string(),
        );
        let width = number(window, cx, String::new());
        let height = number(window, cx, String::new());
        let args = cx.new(|cx| TextareaState::new(window, cx));
        let current = match &model.read(cx).entry(&id) {
            Some(Entry::Healthy(instance)) => choice_of(instance),
            _ => IconChoice::None,
        };
        let icon_picker = cx.new(|cx| IconPicker::new(current, cx));
        let java_list = java_list(cx);

        let _subscriptions = vec![
            cx.subscribe_in(&name, window, Self::commit_on_blur(Self::commit_name)),
            cx.subscribe_in(&min_mem, window, Self::commit_on_blur(Self::commit_memory)),
            cx.subscribe_in(&max_mem, window, Self::commit_on_blur(Self::commit_memory)),
            cx.subscribe_in(&width, window, Self::commit_on_blur(Self::commit_window)),
            cx.subscribe_in(&height, window, Self::commit_on_blur(Self::commit_window)),
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
            cx.subscribe_in(
                &icon_picker,
                window,
                |this, _, IconPicked(choice): &IconPicked, _, cx| {
                    this.save_icon(choice.clone(), cx)
                },
            ),
            // Notified from inside a window update, `observe_in` would skip
            // the call: note it and follow up from `render`.
            cx.observe(&model, |this, _, cx| {
                this.stale = true;
                cx.notify();
            }),
            cx.observe(&java_list, |this, _, cx| {
                this.stale = true;
                cx.notify();
            }),
        ];
        let mut this = Self {
            model,
            id,
            name,
            min_mem,
            max_mem,
            args,
            width,
            height,
            icon_picker,
            icon_open: false,
            java_list,
            probed: HashMap::new(),
            probe_started: HashSet::new(),
            probing: Vec::new(),
            queue: VecDeque::new(),
            saving: false,
            idle_waiters: Vec::new(),
            discarded: Arc::new(Mutex::new(None)),
            gc_reset_for: None,
            required_done: false,
            stale: false,
            errors: Errors::default(),
            scroll: ScrollHandle::new(),
            seen: None,
            required: None,
            required_mc: None,
            required_revision: Revision::default(),
            required_task: Task::ready(()),
            early: Early::Loading,
            early_dir: None,
            early_revision: Revision::default(),
            early_task: Task::ready(()),
            deleting: false,
            _subscriptions,
        };
        this.apply_config(window, cx, true);
        this
    }

    /// A subscription to an input that commits when it loses focus; the
    /// single-line ones also on Enter.
    fn commit_on_blur(
        commit: fn(&mut Self, &mut Window, &mut Context<Self>),
    ) -> impl Fn(&mut Self, &Entity<InputState>, &InputEvent, &mut Window, &mut Context<Self>) + 'static
    {
        move |this, _, event, window, cx| {
            if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                commit(this, window, cx);
            }
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.id
    }

    /// The JVM arguments field.
    pub fn args_input(&self) -> &Entity<TextareaState> {
        &self.args
    }

    /// The error shown under the JVM arguments, if any.
    pub fn args_error(&self) -> Option<&SharedString> {
        self.errors.args.as_ref()
    }

    /// The instance as listed.
    fn instance(&self, cx: &App) -> Option<Instance> {
        match self.model.read(cx).entry(&self.id) {
            Some(Entry::Healthy(instance)) => Some(instance.clone()),
            _ => None,
        }
    }

    fn config(&self, cx: &App) -> Option<InstanceConfig> {
        self.instance(cx).map(|i| i.config)
    }

    /// Why renaming and deleting are disabled now, or `None`.
    fn busy_reason(&self, cx: &App) -> Option<String> {
        self.model.read(cx).busy_reason(&self.id)
    }

    // --- following the saved config ----------------------------------------

    /// Puts `text` in `input` unless it holds it already, or the user is in
    /// it and `force` is off.
    fn set_text(
        input: &Entity<InputState>,
        text: &str,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if input.read(cx).value() == text || (!force && input.focus_handle(cx).is_focused(window)) {
            return;
        }
        input.update(cx, |input, cx| {
            input.set_value(text.to_string(), window, cx)
        });
    }

    /// Sets the inputs from the instance's saved config when that changed
    /// (or `force`), leaving the one being typed in alone, and starts what
    /// depends on the config: the Java list, the early-window state.
    fn apply_config(&mut self, window: &mut Window, cx: &mut Context<Self>, force: bool) {
        let Some(instance) = self.instance(cx) else {
            return;
        };
        let config = instance.config.clone();
        if !force && self.seen.as_ref() == Some(&config) {
            return;
        }
        self.seen = Some(config.clone());
        let java = config.java_override.clone().unwrap_or_default();
        Self::set_text(&self.name, &config.name, force, window, cx);
        Self::set_text(
            &self.min_mem,
            &java.min_mem_mib.to_string(),
            force,
            window,
            cx,
        );
        let max = java.max_mem_mib.map(|m| m.to_string()).unwrap_or_default();
        Self::set_text(&self.max_mem, &max, force, window, cx);
        let size = config
            .window
            .map_or(DEFAULT_WINDOW, |w| (w.width, w.height));
        Self::set_text(&self.width, &size.0.to_string(), force, window, cx);
        Self::set_text(&self.height, &size.1.to_string(), force, window, cx);
        if self.args.read(cx).value() != java.extra_jvm_args.as_str()
            && (force || !self.args.focus_handle(cx).is_focused(window))
        {
            self.args.update(cx, |args, cx| {
                args.set_value(java.extra_jvm_args.clone(), window, cx)
            });
            self.errors.args = None;
        }
        self.sync_required(config.mc_version(), window, cx);
        let neoforge = matches!(config.loader(), Some((LoaderKind::Neoforge, _)));
        self.sync_early(instance.game_dir(), neoforge, cx);
        if config.java_override.is_some() {
            detect_java(&self.java_list, cx);
        }
        if let Some(path) = java.java_path {
            self.probe_unknown(path, window, cx);
        }
        self.revalidate_gc(window, cx);
    }

    /// Runs every pending commit, as leaving a field would: a valid edit is
    /// saved, an invalid one stays in its field with its error. The
    /// future resolves once all saves have reached the disk.
    pub fn flush(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl Future<Output = ()> + use<> {
        self.commit_name(window, cx);
        self.commit_memory(window, cx);
        self.commit_window(window, cx);
        self.commit_args(window, cx);
        let (done, wait) = oneshot::channel();
        if self.saving || !self.queue.is_empty() {
            self.idle_waiters.push(done);
        } else {
            done.send(()).ok();
        }
        async move {
            wait.await.ok();
        }
    }

    /// Resets a saved GC preset the Java now known cannot run, with a
    /// notice.
    fn revalidate_gc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || !self.queue.is_empty() {
            return;
        }
        let Some(config) = self.config(cx) else {
            return;
        };
        let Some(java) = &config.java_override else {
            return;
        };
        let Some(major) = self.java_major(java, cx) else {
            return;
        };
        let key = (java.java_path.clone(), java.gc_preset);
        if java.gc_preset.is_compatible(major) || self.gc_reset_for.as_ref() == Some(&key) {
            return;
        }
        self.gc_reset_for = Some(key);
        let text = t!(
            "instance_settings.gc_reset",
            preset = java.gc_preset.display_name(),
            major = major
        );
        window.push_notification(Notification::warning(text.to_string()), cx);
        self.set_gc(GcPreset::None, window, cx);
    }

    /// Why the GC options needing a known Java are unavailable, if the
    /// Java major is not known.
    fn java_unknown(&self, java: &JavaSettings, cx: &App) -> Option<JavaUnknown> {
        if self.java_major(java, cx).is_some() {
            return None;
        }
        let done = match &java.java_path {
            Some(path) => {
                self.java_list.read(cx).installs.is_some() && self.probed.contains_key(path)
            }
            None => self.required_done,
        };
        Some(if done {
            JavaUnknown::Unknown
        } else {
            JavaUnknown::Checking
        })
    }

    /// Reads the Java major the instance's Minecraft version needs.
    fn sync_required(&mut self, mc: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.required_mc.as_deref() == Some(mc) {
            return;
        }
        self.required_mc = Some(mc.to_string());
        self.required = None;
        self.required_done = false;
        let request = self.required_revision.start();
        let versions = Core::global(cx).data.versions();
        let mc = mc.to_string();
        let job = run_blocking(cx, move || overview::required_java(&versions, &mc));
        self.required_task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update_in(cx, |this, window, cx| {
                if this.required_revision.is_current(request) {
                    this.required_done = true;
                    if let Ok(java) = result {
                        this.required = java;
                    }
                    this.revalidate_gc(window, cx);
                    cx.notify();
                }
            })
            .ok();
        });
    }

    /// Reads whether the early window is disabled, for a NeoForge instance.
    fn sync_early(&mut self, game_dir: PathBuf, neoforge: bool, cx: &mut Context<Self>) {
        if !neoforge {
            self.early_dir = None;
            return;
        }
        if self.early_dir.as_ref() == Some(&game_dir) {
            return;
        }
        self.early_dir = Some(game_dir.clone());
        self.early = Early::Loading;
        self.errors.neoforge = None;
        let request = self.early_revision.start();
        let job = run_blocking(cx, move || {
            patina_core::instance_files::early_window_disabled(&game_dir)
        });
        self.early_task = cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if !this.early_revision.is_current(request) {
                    return;
                }
                match result {
                    Ok(Ok(disabled)) => this.early = Early::Value(disabled),
                    Ok(Err(error)) => {
                        this.early = Early::Failed;
                        this.errors.neoforge = Some(
                            t!("instance_settings.early_read_failed", error = error)
                                .to_string()
                                .into(),
                        );
                    }
                    Err(_) => this.early = Early::Failed,
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Finds the major of `path` by running it, unless a detected install or
    /// an earlier look covers it.
    fn probe_unknown(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !self.probe_started.insert(path.clone()) || self.detected_major(&path, cx).is_some() {
            return;
        }
        let job = run_blocking(cx, {
            let path = path.clone();
            move || probe(&path).map(|install| install.major)
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let major = job.await.ok().flatten();
            this.update_in(cx, |this, window, cx| {
                this.probed.insert(path, major);
                this.revalidate_gc(window, cx);
                cx.notify();
            })
            .ok();
        });
        self.probing.push(task);
    }

    fn detected_major(&self, path: &PathBuf, cx: &App) -> Option<u32> {
        let list = self.java_list.read(cx);
        let installs = list.installs.as_deref().unwrap_or_default();
        installs.iter().find(|i| &i.path == path).map(|i| i.major)
    }

    /// The Java major the instance will run on, when known: the chosen
    /// path's, or what its Minecraft version asks of the managed runtime.
    fn java_major(&self, java: &JavaSettings, cx: &App) -> Option<u32> {
        match &java.java_path {
            Some(path) => self
                .detected_major(path, cx)
                .or_else(|| self.probed.get(path).copied().flatten()),
            None => self.required,
        }
    }

    // --- saving -------------------------------------------------------------

    /// Runs `job` (a core call on the store) in the background, then
    /// refreshes the model. A failure shows the core's error for `field`
    /// and puts the inputs back to the saved config.
    fn save(
        &mut self,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
        job: impl FnOnce(&InstanceStore, &str) -> Result<(), InstanceError> + Send + 'static,
    ) {
        self.queue.push_back((field, Box::new(job)));
        self.pump(window, cx);
    }

    /// Starts the next queued save unless one is in flight: saves run one
    /// at a time, so two quick commits cannot interleave their
    /// load-change-save.
    fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some((field, job)) = self.queue.pop_front() else {
            for waiter in self.idle_waiters.drain(..) {
                waiter.send(()).ok();
            }
            return;
        };
        self.saving = true;
        let store = Core::global(cx).store.clone();
        let id = self.id.clone();
        let job = run_blocking(cx, move || job(&store, &id));
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => {
                        tracing::warn!(%error, ?field, "saving an instance setting failed");
                        Some(error.to_string())
                    }
                    Err(error) => Some(error.to_string()),
                };
                *this.errors.slot(field) = error.clone().map(Into::into);
                if error.is_some() {
                    this.seen = None;
                }
                this.model.update(cx, |model, cx| model.refresh(cx));
                this.apply_config(window, cx, false);
                cx.notify();
                this.pump(window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Changes the saved config with `change`, read fresh from disk.
    fn update_config(
        &mut self,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut InstanceConfig) + Send + 'static,
    ) {
        self.save(field, window, cx, move |store, id| {
            let mut instance = store.load(id)?;
            change(&mut instance.config);
            store.save(&instance)
        });
    }

    fn commit_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.config(cx) else {
            return;
        };
        let text = self.name.read(cx).value().trim().to_string();
        if text == config.name {
            self.errors.name = None;
            cx.notify();
            return;
        }
        if self.busy_reason(cx).is_some() {
            Self::set_text(&self.name, &config.name, true, window, cx);
            return;
        }
        if text.is_empty() {
            self.errors.name = Some(t!("instance_settings.name_empty").to_string().into());
            cx.notify();
            return;
        }
        self.errors.name = None;
        self.save(Field::Name, window, cx, move |store, id| {
            store.rename(id, &text).map(drop)
        });
    }

    /// Reads the memory inputs: the minimum, and the maximum (`None` for
    /// automatic), or the reason they are not acceptable.
    fn read_memory(&self, cx: &App) -> Result<(u32, Option<u32>), String> {
        let integer = || t!("instance_settings.mem_integer").to_string();
        let min = self.min_mem.read(cx).value();
        let min: u32 = min.trim().parse().map_err(|_| integer())?;
        let too_big = || t!("instance_settings.mem_too_big", max = MAX_MEM_MIB).to_string();
        if min == 0 {
            return Err(t!("instance_settings.mem_min_zero").to_string());
        }
        if min > MAX_MEM_MIB {
            return Err(too_big());
        }
        let max = self.max_mem.read(cx).value();
        let max = match max.trim() {
            "" => None,
            text => Some(text.parse::<u32>().map_err(|_| integer())?),
        };
        if max.is_some_and(|max| max > MAX_MEM_MIB) {
            return Err(too_big());
        }
        if max.is_some_and(|max| min > max) {
            return Err(t!("instance_settings.mem_min_over_max").to_string());
        }
        Ok((min, max))
    }

    fn commit_memory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.config(cx) else {
            return;
        };
        // Nothing to commit while the global settings apply.
        if config.java_override.is_none() {
            return;
        }
        let (min, max) = match self.read_memory(cx) {
            Ok(memory) => memory,
            Err(reason) => {
                self.errors.memory = Some(reason.into());
                cx.notify();
                return;
            }
        };
        self.errors.memory = None;
        cx.notify();
        let saved = config.java_override.as_ref();
        if saved.is_none_or(|j| j.min_mem_mib == min && j.max_mem_mib == max) {
            return;
        }
        self.update_config(Field::Memory, window, cx, move |config| {
            if let Some(java) = &mut config.java_override {
                java.min_mem_mib = min;
                java.max_mem_mib = max;
            }
        });
    }

    /// "自动" for the maximum.
    fn max_auto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        Self::set_text(&self.max_mem, "", true, window, cx);
        self.commit_memory(window, cx);
    }

    fn args_changed(&mut self, cx: &mut Context<Self>) {
        let text = self.args.read(cx).value();
        self.errors.args = validate_extra_jvm_args(&text)
            .err()
            .map(|error| error.to_string().into());
        cx.notify();
    }

    fn commit_args(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.config(cx) else {
            return;
        };
        if config.java_override.is_none() {
            return;
        }
        let text = self.args.read(cx).value().to_string();
        if validate_extra_jvm_args(&text).is_err() {
            self.args_changed(cx);
            return;
        }
        let saved = config.java_override.as_ref();
        if saved.is_none_or(|j| j.extra_jvm_args == text) {
            return;
        }
        self.update_config(Field::Args, window, cx, move |config| {
            if let Some(java) = &mut config.java_override {
                java.extra_jvm_args = text;
            }
        });
    }

    fn set_gc(&mut self, preset: GcPreset, window: &mut Window, cx: &mut Context<Self>) {
        // A preset the user picks next may get a notice again.
        if preset != GcPreset::None {
            self.gc_reset_for = None;
        }
        self.update_config(Field::Gc, window, cx, move |config| {
            if let Some(java) = &mut config.java_override {
                java.gc_preset = preset;
            }
        });
    }

    /// Uses `path` as the Java (`None`: the managed runtime) whose major is
    /// `major`; a GC preset that major cannot run is reset.
    fn set_java(
        &mut self,
        path: Option<PathBuf>,
        major: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.errors.java = None;
        self.update_config(Field::Java, window, cx, move |config| {
            if let Some(java) = &mut config.java_override {
                java.java_path = path;
                if major.is_some_and(|m| !java.gc_preset.is_compatible(m)) {
                    java.gc_preset = GcPreset::None;
                }
            }
        });
    }

    /// "选择文件…": asks for a Java executable, then checks and uses it.
    fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                this.update_in(cx, |this, window, cx| this.use_java_file(path, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Checks that `path` runs as a Java, then uses it. Not a Java: says
    /// so and saves nothing.
    pub fn use_java_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let job = run_blocking(cx, {
            let path = path.clone();
            move || probe(&path)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(Some(install)) => {
                    this.probed.insert(path.clone(), Some(install.major));
                    this.set_java(Some(path), Some(install.major), window, cx);
                }
                _ => {
                    this.errors.java =
                        Some(t!("instance_settings.java_not_java").to_string().into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Turns "使用全局设置" on (no override; the override is kept for
    /// turning it off again) or off (the kept override, else one seeded
    /// from the global config).
    fn set_use_global(&mut self, global: bool, window: &mut Window, cx: &mut Context<Self>) {
        // What was typed belongs to the override that is going away.
        if global {
            drop(self.flush(window, cx));
            let kept = self.discarded.clone();
            self.update_config(Field::Global, window, cx, move |config| {
                if let Some(old) = config.java_override.take() {
                    *kept.lock().unwrap_or_else(|e| e.into_inner()) = Some(old);
                }
            });
            return;
        }
        let config = Core::read_config(cx);
        let kept = self.discarded.clone();
        cx.spawn_in(window, async move |this, cx| {
            let seed = config.await.map(|c| c.java).unwrap_or_default();
            this.update_in(cx, |this, window, cx| {
                this.update_config(Field::Global, window, cx, move |config| {
                    if config.java_override.is_none() {
                        let old = kept.lock().unwrap_or_else(|e| e.into_inner()).take();
                        config.java_override = Some(old.unwrap_or(seed));
                    }
                });
            })
            .ok();
        })
        .detach();
    }

    fn set_window_mode(&mut self, custom: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.errors.window = None;
        if !custom {
            self.update_config(Field::Window, window, cx, |config| config.window = None);
            return;
        }
        let size = self.read_window(cx).unwrap_or(WindowSize {
            width: DEFAULT_WINDOW.0,
            height: DEFAULT_WINDOW.1,
        });
        self.update_config(Field::Window, window, cx, move |config| {
            config.window = Some(size);
        });
    }

    /// The window size in the inputs, if both are within the core's range.
    fn read_window(&self, cx: &App) -> Option<WindowSize> {
        let parse = |input: &Entity<InputState>| input.read(cx).value().trim().parse::<u32>().ok();
        let (width, height) = (parse(&self.width)?, parse(&self.height)?);
        let width_ok = (WindowSize::MIN_WIDTH..=WindowSize::MAX).contains(&width);
        let height_ok = (WindowSize::MIN_HEIGHT..=WindowSize::MAX).contains(&height);
        (width_ok && height_ok).then_some(WindowSize { width, height })
    }

    fn commit_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.config(cx) else {
            return;
        };
        // Only a custom size has inputs to commit.
        let Some(saved) = config.window else {
            return;
        };
        let Some(size) = self.read_window(cx) else {
            self.errors.window = Some(
                t!(
                    "instance_settings.window_range",
                    min_w = WindowSize::MIN_WIDTH,
                    min_h = WindowSize::MIN_HEIGHT,
                    max = WindowSize::MAX
                )
                .to_string()
                .into(),
            );
            cx.notify();
            return;
        };
        self.errors.window = None;
        cx.notify();
        if size != saved {
            self.update_config(Field::Window, window, cx, move |config| {
                config.window = Some(size);
            });
        }
    }

    fn save_icon(&mut self, choice: IconChoice, cx: &mut Context<Self>) {
        let task = super::save_icon(&self.model, self.id.clone(), choice, cx);
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |this, cx| {
                    this.icon_picker
                        .update(cx, |picker, cx| picker.set_error(Some(error), cx));
                })
                .ok();
            }
        })
        .detach();
    }

    /// Turns the NeoForge early window off (`disabled`) or on.
    fn set_early(&mut self, disabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Early::Value(before), Some(dir)) = (self.early.clone(), self.early_dir.clone()) else {
            return;
        };
        self.early = Early::Value(disabled);
        self.errors.neoforge = None;
        let job = run_blocking(cx, move || {
            patina_core::instance_files::set_early_window_disabled(&dir, disabled)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update_in(cx, |this, _, cx| {
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(error) => Some(error.to_string()),
                };
                if let Some(error) = error {
                    this.early = Early::Value(before);
                    this.errors.neoforge = Some(
                        t!("instance_settings.early_write_failed", error = error)
                            .to_string()
                            .into(),
                    );
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Deletes the instance, then selects the entry next to it.
    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.deleting || self.busy_reason(cx).is_some() {
            return;
        }
        self.deleting = true;
        let model = self.model.clone();
        // The entry after it, else the one before.
        let next = {
            let listed = model.read(cx).instances();
            listed.iter().position(|i| i.id == self.id).and_then(|at| {
                let neighbour = listed
                    .get(at + 1)
                    .or_else(|| at.checked_sub(1).and_then(|p| listed.get(p)));
                neighbour.map(|i| i.id.clone())
            })
        };
        let name = model
            .read(cx)
            .display_name(&self.id)
            .unwrap_or_else(|| self.id.clone());
        let store = Core::global(cx).store.clone();
        let id = self.id.clone();
        let job = run_blocking(cx, move || store.delete(&id));
        cx.spawn_in(window, async move |this, cx| {
            let result = match job.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            match result {
                Ok(()) => {
                    let wait = cx.update(|_, cx| {
                        model.update(cx, |model, cx| {
                            cx.emit(InstancesChanged);
                            model.refresh_and_wait(cx)
                        })
                    });
                    if let Ok(wait) = wait {
                        wait.await;
                    }
                    cx.update(|_, cx| {
                        if let Some(next) = &next {
                            model.update(cx, |model, cx| model.select(next, cx));
                        }
                    })
                    .ok();
                }
                Err(error) => {
                    tracing::warn!(%error, "deleting an instance failed");
                    this.update_in(cx, |this, window, cx| {
                        this.deleting = false;
                        let text = t!(
                            "instance_settings.delete_failed",
                            name = name,
                            error = error
                        );
                        window.push_notification(Notification::error(text.to_string()), cx);
                        cx.notify();
                    })
                    .ok();
                    // It may be gone or changed anyway.
                    cx.update(|_, cx| model.update(cx, |model, cx| model.refresh(cx)))
                        .ok();
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .model
            .read(cx)
            .display_name(&self.id)
            .unwrap_or_else(|| self.id.clone());
        let form = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let title = SharedString::from(t!("instance_settings.delete_title", name = name));
            let form = form.clone();
            alert
                .title(
                    div()
                        .id("settings-delete-title")
                        .aria_label(title.clone())
                        .child(title)
                        .test_support(),
                )
                .description(t!("instance_settings.delete_body").to_string())
                .confirm()
                .ok_text(t!("instance_settings.delete_confirm").to_string())
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("instance_settings.delete_cancel").to_string())
                .on_ok(move |_, window, cx| {
                    form.update(cx, |form, cx| form.delete(window, cx));
                    true
                })
        });
    }
}

/// The picker's choice for `instance`'s icon.
fn choice_of(instance: &Instance) -> IconChoice {
    match &instance.config.icon {
        Some(InstanceIcon::Builtin(name)) => IconChoice::Builtin(name.clone()),
        Some(InstanceIcon::Custom) => instance
            .icon_path()
            .map_or(IconChoice::None, IconChoice::Custom),
        None => IconChoice::None,
    }
}

// --- drawing ------------------------------------------------------------------

/// One entry of the Java choice.
struct JavaOption {
    /// `None`: the managed runtime.
    path: Option<PathBuf>,
    label: String,
}

impl SettingsForm {
    fn java_options(&self, current: Option<&PathBuf>, cx: &App) -> Vec<JavaOption> {
        let installs = self
            .java_list
            .read(cx)
            .installs
            .as_deref()
            .unwrap_or_default();
        let mut options = vec![JavaOption {
            path: None,
            label: t!("instance_settings.java_auto").to_string(),
        }];
        options.extend(installs.iter().map(|install| {
            JavaOption {
                path: Some(install.path.clone()),
                label: t!(
                    "instance_settings.java_option",
                    major = install.major,
                    version = install.version,
                    path = install.path.display()
                )
                .to_string(),
            }
        }));
        // A saved or chosen file the detection did not list.
        if let Some(path) = current
            && !installs.iter().any(|i| &i.path == path)
        {
            options.push(JavaOption {
                path: Some(path.clone()),
                label: path.display().to_string(),
            });
        }
        options
    }

    fn heading(text: String, cx: &App) -> impl IntoElement {
        div()
            .text_size(TEXT_ROW)
            .strong(FontWeight::SEMIBOLD, cx)
            .text_color(cx.theme().foreground)
            .child(text)
    }

    fn label(text: String, cx: &App) -> impl IntoElement {
        div()
            .text_size(TEXT_SECONDARY)
            .strong(FontWeight::MEDIUM, cx)
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }

    fn hint(id: &'static str, text: String, cx: &App) -> impl IntoElement {
        div()
            .id(id)
            .test_support()
            .aria_label(text.clone())
            .text_size(TEXT_SECONDARY)
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }

    /// The reason under a field, in the danger colour.
    fn error(id: &'static str, text: Option<&SharedString>, cx: &App) -> Option<impl IntoElement> {
        text.map(|text| {
            div()
                .id(id)
                .test_support()
                .aria_label(text.clone())
                .text_size(TEXT_SECONDARY)
                .text_color(cx.theme().danger)
                .child(text.clone())
        })
    }

    /// A section: a heading, then its fields, closed by a hairline.
    fn section(
        id: &'static str,
        title: String,
        cx: &App,
    ) -> impl ParentElement + Styled + IntoElement + use<> {
        v_flex()
            .id(id)
            .test_support()
            .w_full()
            .gap_3()
            .py_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(Self::heading(title, cx))
    }

    fn field(label: String, cx: &App) -> gpui_kit::Div {
        v_flex().gap_1().child(Self::label(label, cx))
    }

    fn render_basics(
        &self,
        instance: &Instance,
        busy: Option<&String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let renamable = busy.is_none();
        Self::section(
            "settings-basics",
            t!("instance_settings.basics").to_string(),
            cx,
        )
        .child(
            Self::field(t!("instance_settings.name").to_string(), cx)
                .child(
                    Input::new(&self.name)
                        .id("settings-name")
                        .disabled(!renamable)
                        .w(rems(20.)),
                )
                .children(busy.map(|reason| Self::hint("settings-name-reason", reason.clone(), cx)))
                .children(Self::error(
                    "settings-name-error",
                    self.errors.name.as_ref(),
                    cx,
                )),
        )
        .child(
            Self::field(t!("instance_settings.icon").to_string(), cx)
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(instance_icon(&icon_for(instance), rems(2.5), cx))
                        .child(
                            Button::new("settings-icon-toggle")
                                .outline()
                                .label(if self.icon_open {
                                    t!("instance_settings.close_icon").to_string()
                                } else {
                                    t!("instance_settings.change_icon").to_string()
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.icon_open = !this.icon_open;
                                    cx.notify();
                                })),
                        ),
                )
                .when(self.icon_open, |f| f.child(self.icon_picker.clone())),
        )
    }

    fn render_java(
        &self,
        config: &InstanceConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let global = config.java_override.is_none();
        let section = Self::section(
            "settings-java-memory",
            t!("instance_settings.java_memory").to_string(),
            cx,
        )
        .child(
            v_flex()
                .gap_1()
                .child(
                    Switch::new("settings-use-global")
                        .checked(global)
                        .label(t!("instance_settings.use_global").to_string())
                        .on_click(cx.listener(|this, checked: &bool, window, cx| {
                            this.set_use_global(*checked, window, cx);
                        })),
                )
                .child(Self::hint(
                    "settings-use-global-hint",
                    t!("instance_settings.use_global_hint").to_string(),
                    cx,
                )),
        );
        let Some(java) = &config.java_override else {
            return section;
        };
        section
            .child(self.render_java_choice(java, cx))
            .child(self.render_memory(cx))
            .child(self.render_gc(java, cx))
            .child(self.render_args(cx))
    }

    fn render_java_choice(
        &self,
        java: &JavaSettings,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let options = self.java_options(java.java_path.as_ref(), cx);
        let selected = options.iter().position(|o| o.path == java.java_path);
        let detecting = self.java_list.read(cx).installs.is_none();
        let radios: Vec<Radio> = options
            .iter()
            .map(|o| Radio::new("java").label(o.label.clone()))
            .collect();
        let paths: Vec<Option<PathBuf>> = options.into_iter().map(|o| o.path).collect();
        Self::field(t!("instance_settings.java").to_string(), cx)
            .child(
                RadioGroup::new("settings-java")
                    .selected_index(selected)
                    .children(radios)
                    .on_click(cx.listener(move |this, ix: &usize, window, cx| {
                        let Some(path) = paths.get(*ix).cloned() else {
                            return;
                        };
                        let major = match &path {
                            Some(path) => this
                                .detected_major(path, cx)
                                .or_else(|| this.probed.get(path).copied().flatten()),
                            None => this.required,
                        };
                        this.set_java(path, major, window, cx);
                    })),
            )
            .when(detecting, |f| {
                f.child(Self::hint(
                    "settings-java-detecting",
                    t!("instance_settings.java_detecting").to_string(),
                    cx,
                ))
            })
            .child(
                h_flex().child(
                    Button::new("settings-java-choose")
                        .outline()
                        .small()
                        .label(t!("instance_settings.java_choose").to_string())
                        .on_click(cx.listener(|this, _, window, cx| this.choose_file(window, cx))),
                ),
            )
            .children(Self::error(
                "settings-java-error",
                self.errors.java.as_ref(),
                cx,
            ))
    }

    fn render_memory(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_4()
                    .items_end()
                    .child(
                        Self::field(t!("instance_settings.min_mem").to_string(), cx).child(
                            Input::new(&self.min_mem)
                                .id("settings-min-mem")
                                .w(rems(NUMBER_W.0)),
                        ),
                    )
                    .child(
                        Self::field(t!("instance_settings.max_mem").to_string(), cx).child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Input::new(&self.max_mem)
                                        .id("settings-max-mem")
                                        .w(rems(NUMBER_W.0)),
                                )
                                .child(
                                    Button::new("settings-max-auto")
                                        .ghost()
                                        .small()
                                        .label(t!("instance_settings.max_auto").to_string())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.max_auto(window, cx);
                                        })),
                                ),
                        ),
                    ),
            )
            .children(Self::error(
                "settings-memory-error",
                self.errors.memory.as_ref(),
                cx,
            ))
    }

    fn render_gc(&self, java: &JavaSettings, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let major = self.java_major(java, cx);
        let unknown = self.java_unknown(java, cx);
        // Without a known Java only the presets that need none are open.
        let compatible = |preset: GcPreset| match major {
            Some(m) => preset.is_compatible(m),
            // Every Java Minecraft runs on is at least 8.
            None => preset.min_java_major() <= 8,
        };
        let radios: Vec<Radio> = GC_PRESETS
            .iter()
            .map(|preset| {
                Radio::new("gc")
                    .label(preset.display_name())
                    .disabled(!compatible(*preset))
            })
            .collect();
        let reasons: Vec<(usize, String)> = GC_PRESETS
            .iter()
            .enumerate()
            .filter(|(_, preset)| !compatible(**preset) && major.is_some())
            .map(|(ix, preset)| {
                let reason = t!(
                    "instance_settings.gc_needs",
                    preset = preset.display_name(),
                    major = preset.min_java_major()
                );
                (ix, reason.to_string())
            })
            .collect();
        Self::field(t!("instance_settings.gc").to_string(), cx)
            .child(
                RadioGroup::new("settings-gc")
                    .selected_index(GC_PRESETS.iter().position(|p| *p == java.gc_preset))
                    .children(radios)
                    .on_click(cx.listener(|this, ix: &usize, window, cx| {
                        if let Some(preset) = GC_PRESETS.get(*ix) {
                            this.set_gc(*preset, window, cx);
                        }
                    })),
            )
            .children(unknown.map(|why| {
                let reason = match why {
                    JavaUnknown::Checking => t!("instance_settings.gc_checking"),
                    JavaUnknown::Unknown => t!("instance_settings.gc_unknown"),
                }
                .to_string();
                Self::hint("settings-gc-unknown", reason, cx)
            }))
            .children(reasons.into_iter().map(|(ix, reason)| {
                div()
                    .id(SharedString::from(format!("settings-gc-reason-{ix}")))
                    .test_support()
                    .aria_label(reason.clone())
                    .text_size(TEXT_SECONDARY)
                    .text_color(cx.theme().muted_foreground)
                    .child(reason)
            }))
    }

    fn render_args(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        Self::field(t!("instance_settings.jvm_args").to_string(), cx)
            .child(
                div()
                    .id("settings-args")
                    .test_support()
                    .child(Textarea::new(&self.args).h(rems(5.5))),
            )
            .child(Self::hint(
                "settings-args-hint",
                t!("instance_settings.jvm_args_hint").to_string(),
                cx,
            ))
            .children(Self::error(
                "settings-args-error",
                self.errors.args.as_ref(),
                cx,
            ))
    }

    fn render_window(
        &self,
        config: &InstanceConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let custom = config.window.is_some();
        Self::section(
            "settings-window",
            t!("instance_settings.window").to_string(),
            cx,
        )
        .child(
            RadioGroup::horizontal("settings-window-mode")
                .selected_index(Some(usize::from(custom)))
                .child(
                    Radio::new("default").label(t!("instance_settings.window_default").to_string()),
                )
                .child(
                    Radio::new("custom").label(t!("instance_settings.window_custom").to_string()),
                )
                .on_click(cx.listener(|this, ix: &usize, window, cx| {
                    this.set_window_mode(*ix == 1, window, cx);
                })),
        )
        .when(custom, |s| {
            s.child(
                h_flex()
                    .gap_4()
                    .child(
                        Self::field(t!("instance_settings.width").to_string(), cx).child(
                            Input::new(&self.width)
                                .id("settings-width")
                                .w(rems(NUMBER_W.0)),
                        ),
                    )
                    .child(
                        Self::field(t!("instance_settings.height").to_string(), cx).child(
                            Input::new(&self.height)
                                .id("settings-height")
                                .w(rems(NUMBER_W.0)),
                        ),
                    ),
            )
        })
        .children(Self::error(
            "settings-window-error",
            self.errors.window.as_ref(),
            cx,
        ))
    }

    fn render_neoforge(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let value = matches!(self.early, Early::Value(true));
        let usable = matches!(self.early, Early::Value(_));
        Self::section(
            "settings-neoforge",
            t!("instance_settings.neoforge").to_string(),
            cx,
        )
        .child(
            Switch::new("settings-early-window")
                .checked(value)
                .disabled(!usable)
                .label(t!("instance_settings.early_window").to_string())
                .on_click(cx.listener(|this, checked: &bool, window, cx| {
                    this.set_early(*checked, window, cx);
                })),
        )
        .child(Self::hint(
            "settings-early-window-hint",
            t!("instance_settings.early_window_hint").to_string(),
            cx,
        ))
        .children(Self::error(
            "settings-neoforge-error",
            self.errors.neoforge.as_ref(),
            cx,
        ))
    }

    fn render_danger(
        &self,
        busy: Option<&String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let disabled = busy.is_some() || self.deleting;
        Self::section(
            "settings-danger",
            t!("instance_settings.danger").to_string(),
            cx,
        )
        .border_b_0()
        .child(
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    Button::new("settings-delete")
                        .danger()
                        .outline()
                        .label(t!("instance_settings.delete").to_string())
                        .loading(self.deleting)
                        .disabled(disabled)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_delete(window, cx);
                        })),
                )
                .children(
                    busy.map(|reason| Self::hint("settings-delete-reason", reason.clone(), cx)),
                ),
        )
    }
}

impl Render for SettingsForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.stale) {
            cx.defer_in(window, |this, window, cx| {
                this.apply_config(window, cx, false);
                this.revalidate_gc(window, cx);
                cx.notify();
            });
        }
        let Some(instance) = self.instance(cx) else {
            return div().id("settings-tab").into_any_element();
        };
        let busy = self.busy_reason(cx);
        let neoforge = matches!(instance.config.loader(), Some((LoaderKind::Neoforge, _)));
        let content = div()
            .id("settings-tab")
            .test_support()
            .text_size(TEXT_PRIMARY)
            .child(
                v_flex()
                    .w_full()
                    .max_w(CONTENT_W)
                    .px(GUTTER)
                    .child(self.render_basics(&instance, busy.as_ref(), cx))
                    .child(self.render_java(&instance.config, cx))
                    .child(self.render_window(&instance.config, cx))
                    .when(neoforge, |d| d.child(self.render_neoforge(cx)))
                    .child(self.render_danger(busy.as_ref(), cx)),
            );
        scroll_y(content, &self.scroll).into_any_element()
    }
}
