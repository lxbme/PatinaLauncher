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

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
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
use patina_core::java::JavaInstall;
use patina_core::jvm::{GcPreset, validate_extra_jvm_args};
use patina_core::loader::LoaderKind;
use rust_i18n::t;

use super::icon::IconChoice;
use super::icon_picker::{IconPicked, IconPicker};
use super::model::{Entry, InstancesChanged, InstancesModel};
use super::{icon_for, instance_icon, observe_activity, overview};
use crate::core_bridge::{Core, Revision, run_blocking};
use crate::fonts::Strong;
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_ROW, TEXT_SECONDARY, scroll_y};

/// Widest the form grows.
const CONTENT_W: Rems = Rems(40.);
/// Width of the numeric inputs.
const NUMBER_W: Rems = Rems(9.);
/// What "自定义" starts from: Minecraft's own default window.
const DEFAULT_WINDOW: (u32, u32) = (854, 480);
/// Largest memory value accepted, in MiB (1 TiB): far beyond any machine
/// today, but it keeps the value inside what the JVM flags can express.
pub const MAX_MEM_MIB: u32 = 1_048_576;
/// The GC presets in the order they are listed.
pub(crate) const GC_PRESETS: [GcPreset; 4] = [
    GcPreset::None,
    GcPreset::G1Aikar,
    GcPreset::GenerationalZgc,
    GcPreset::Custom,
];

// --- the rules the global settings share -------------------------------------

/// Reads the memory fields: the minimum, and the maximum (`None`, an empty
/// field: automatic), or the localized reason they are not acceptable.
/// Whole MiB, the minimum at least 1, neither above [`MAX_MEM_MIB`], and
/// the minimum not above the maximum.
pub(crate) fn parse_memory(min: &str, max: &str) -> Result<(u32, Option<u32>), String> {
    let integer = || t!("instance_settings.mem_integer").to_string();
    let min: u32 = min.trim().parse().map_err(|_| integer())?;
    let too_big = || t!("instance_settings.mem_too_big", max = MAX_MEM_MIB).to_string();
    if min == 0 {
        return Err(t!("instance_settings.mem_min_zero").to_string());
    }
    if min > MAX_MEM_MIB {
        return Err(too_big());
    }
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

/// Whether GC `preset` may be chosen for a Java whose major is `major`.
/// Without a known Java only the presets every Java Minecraft runs on (8
/// and newer) can.
fn gc_selectable(preset: GcPreset, major: Option<u32>) -> bool {
    match major {
        Some(m) => preset.is_compatible(m),
        None => preset.min_java_major() <= 8,
    }
}

// --- the detected Java installs, shared by the session ---------------------

/// The session's Java selector union and read-only system classification,
/// built from detection plus verified owned markers on the blocking pool.
/// Forms share this inventory and refresh it through [`redetect_java`].
pub struct JavaList {
    /// Selector union; ownership is not inferred from membership in this list.
    installs: Option<Vec<JavaInstall>>,
    system_installs: Option<Vec<JavaInstall>>,
    presentation: HashMap<PathBuf, crate::java_settings::JavaOption>,
    aliases: HashMap<PathBuf, PathBuf>,
    loading: bool,
    refresh_pending: bool,
}

impl JavaList {
    /// The installs found, once looked for.
    pub fn installs(&self) -> Option<&[JavaInstall]> {
        self.installs.as_deref()
    }

    /// Read-only detected installs, excluding canonical aliases of owned JDKs.
    pub(crate) fn system_installs(&self) -> Option<&[JavaInstall]> {
        self.system_installs.as_deref()
    }

    pub(crate) fn canonical_path<'a>(&'a self, path: &'a PathBuf) -> &'a PathBuf {
        self.aliases.get(path).unwrap_or(path)
    }
    pub(crate) fn matches(&self, a: Option<&PathBuf>, b: Option<&PathBuf>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => self.canonical_path(a) == self.canonical_path(b),
            _ => false,
        }
    }
    pub(crate) fn remember_probe(
        &mut self,
        path: PathBuf,
        install: JavaInstall,
        vendor: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.aliases.insert(path, install.path.clone());
        self.presentation
            .entry(install.path.clone())
            .or_insert_with(|| crate::java_settings::JavaOption {
                path: Some(install.path),
                vendor,
                version: install.version,
                source: crate::java_settings::JavaSource::Manual,
                selectable: true,
            });
        cx.notify();
    }

    /// Whether a look is under way.
    pub fn is_loading(&self) -> bool {
        self.loading
    }
}

struct JavaListGlobal(Entity<JavaList>);

impl Global for JavaListGlobal {}

/// The session's Java list.
pub(crate) fn java_list(cx: &mut App) -> Entity<JavaList> {
    if let Some(global) = cx.try_global::<JavaListGlobal>() {
        return global.0.clone();
    }
    let list = cx.new(|_| JavaList {
        installs: None,
        system_installs: None,
        presentation: HashMap::new(),
        aliases: HashMap::new(),
        loading: false,
        refresh_pending: false,
    });
    cx.set_global(JavaListGlobal(list.clone()));
    list
}

/// Makes `installs` the detected Java installs, so no detection runs (tests).
pub fn seed_java_list(installs: Vec<JavaInstall>, cx: &mut App) {
    let list = java_list(cx);
    list.update(cx, |list, cx| {
        list.system_installs = Some(installs.clone());
        list.installs = Some(installs);
        list.loading = false;
        list.refresh_pending = false;
        cx.notify();
    });
}

/// Starts looking for Java installs unless that was done or is under way.
pub(crate) fn detect_java(list: &Entity<JavaList>, cx: &mut App) {
    if list.read(cx).installs.is_some() || list.read(cx).loading {
        return;
    }
    redetect_java(list, cx);
}

/// Refreshes Java discovery. A refresh requested during an existing scan is
/// coalesced and repeated afterward, never lost on download/delete completion.
pub(crate) fn redetect_java(list: &Entity<JavaList>, cx: &mut App) {
    if list.read(cx).loading {
        list.update(cx, |list, _| list.refresh_pending = true);
        return;
    }
    list.update(cx, |list, cx| {
        list.loading = true;
        cx.notify();
    });
    let detect = Core::global(cx).detect_java.clone();
    let managed = Core::global(cx).java_distributions.clone();
    let mojang = patina_core::java::ManagedRuntimes::new(Core::global(cx).data.runtimes());
    let job = run_blocking(cx, move || {
        use crate::java_settings::{JavaOption, JavaSource};
        let found = detect();
        let owned = managed.list()?;
        let mut presentation = HashMap::new();
        let mut aliases = HashMap::new();
        for install in &found {
            aliases.insert(
                install.path.clone(),
                install
                    .path
                    .canonicalize()
                    .unwrap_or_else(|_| install.path.clone()),
            );
        }
        let mut managed_installs = Vec::new();
        for owned in owned {
            let java = owned.java().clone();
            let release = owned.release();
            presentation.insert(
                java.path.clone(),
                JavaOption {
                    path: Some(java.path.clone()),
                    vendor: Some(release.vendor().name().into()),
                    version: release.version().into(),
                    source: JavaSource::Vendor(release.id()),
                    selectable: true,
                },
            );
            managed_installs.push(java);
        }
        for (component, version) in mojang.list_installed() {
            let path = mojang.java_bin(&component);
            let probed = patina_core::java::probe(&path);
            let selectable = probed.is_some();
            let java = probed.unwrap_or_else(|| JavaInstall {
                path: path.clone(),
                major: patina_core::java::parse_major(&version).unwrap_or(0),
                version: version.clone(),
                is_64bit: true,
            });
            aliases.insert(path, java.path.clone());
            if !presentation.contains_key(&java.path) {
                presentation.insert(
                    java.path.clone(),
                    JavaOption {
                        path: Some(java.path.clone()),
                        vendor: Some("Mojang".into()),
                        version,
                        source: JavaSource::Mojang(component),
                        selectable,
                    },
                );
                managed_installs.push(java);
            }
        }
        let system_installs = found
            .into_iter()
            .filter_map(|mut java| {
                java.path = aliases.get(&java.path).cloned().unwrap_or(java.path);
                if presentation.contains_key(&java.path) {
                    return None;
                }
                let vendor = crate::java_settings::vendor_name(&java.path);
                presentation.insert(
                    java.path.clone(),
                    JavaOption {
                        path: Some(java.path.clone()),
                        vendor,
                        version: java.version.clone(),
                        source: JavaSource::System,
                        selectable: true,
                    },
                );
                Some(java)
            })
            .collect::<Vec<_>>();
        let mut installs = system_installs.clone();
        installs.extend(managed_installs);
        let mut seen = HashSet::new();
        installs.retain(|i| seen.insert(i.path.clone()));
        installs.sort_by(|a, b| b.major.cmp(&a.major).then_with(|| a.path.cmp(&b.path)));
        Ok::<_, patina_core::java::distributions::DistributionError>((
            installs,
            system_installs,
            presentation,
            aliases,
        ))
    });
    let list = list.clone();
    cx.spawn(async move |cx| {
        let found = job.await.ok().and_then(|result| {
            result
                .inspect_err(|error| tracing::warn!(%error,"Java inventory refresh failed"))
                .ok()
        });
        let repeat = list.update(cx, |list, cx| {
            list.loading = false;
            // Keep both classifications coherent on a failed owned-store read.
            if let Some((installs, system_installs, presentation, aliases)) = found {
                list.installs = Some(installs);
                list.system_installs = Some(system_installs);
                list.presentation.retain(|_, option| {
                    matches!(option.source, crate::java_settings::JavaSource::Manual)
                });
                list.presentation.extend(presentation);
                list.aliases.extend(aliases);
            }
            list.installs.get_or_insert_with(Vec::new);
            list.system_installs.get_or_insert_with(Vec::new);
            let repeat = std::mem::take(&mut list.refresh_pending);
            cx.notify();
            repeat
        });
        if repeat {
            cx.update(|cx| redetect_java(&list, cx));
        }
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
    java_choice_revision: u64,
    /// The pending path keeps Automatic actionable while a file is checked.
    java_file_pending: Option<PathBuf>,
    /// Paths a probe was started for.
    probe_started: HashSet<PathBuf>,
    probing: Vec<Task<()>>,
    /// Saves waiting for the one in flight, run in order.
    queue: VecDeque<(Field, SaveJob)>,
    saving: bool,
    /// Latest accepted intent, kept until matching readback or its final write fails.
    requested_memory: Option<(u32, Option<u32>)>,
    requested_args: Option<String>,
    requested_name: Option<String>,
    requested_window: Option<Option<WindowSize>>,
    requested_java: Option<Option<PathBuf>>,
    requested_gc: Option<GcPreset>,
    requested_global: Option<bool>,
    mode_revision: Revision,
    /// An override awaiting global defaults must also be drained on close.
    mode_seed_pending: bool,
    /// Queued restorations keep their position across mode revisions. Only the
    /// latest seed read resolves these dependencies and releases the pump.
    pending_restore_seeds: Vec<Arc<Mutex<JavaSettings>>>,
    /// Resolved once no save is queued or in flight.
    idle_waiters: Vec<oneshot::Sender<()>>,
    /// The override "使用全局设置" turned off, kept for turning it on again.
    discarded: Arc<Mutex<Option<JavaSettings>>>,
    /// The Java and GC preset a reset notice was already given for.
    gc_reset_for: Option<(Option<PathBuf>, GcPreset)>,
    gc_details_open: bool,
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
        let number =
            |window: &mut Window, cx: &mut Context<Self>| cx.new(|cx| InputState::new(window, cx));
        let name = cx.new(|cx| InputState::new(window, cx));
        let min_mem = number(window, cx);
        let max_mem = number(window, cx);
        let auto = "instance_settings.max_auto_placeholder";
        crate::i18n::localized_placeholder(&max_mem, auto, window, cx);
        let width = number(window, cx);
        let height = number(window, cx);
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
            // Busy or not, and why: only what is enabled changes.
            {
                let id = id.clone();
                let activity = model.read(cx).activity().clone();
                observe_activity(&activity, cx, move |a| a.busy_reason(&id))
            },
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
            java_choice_revision: 0,
            java_file_pending: None,
            probe_started: HashSet::new(),
            probing: Vec::new(),
            queue: VecDeque::new(),
            saving: false,
            requested_memory: None,
            requested_args: None,
            requested_name: None,
            requested_window: None,
            requested_java: None,
            requested_gc: None,
            requested_global: None,
            mode_revision: Revision::default(),
            mode_seed_pending: false,
            pending_restore_seeds: Vec::new(),
            idle_waiters: Vec::new(),
            discarded: Arc::new(Mutex::new(None)),
            gc_reset_for: None,
            gc_details_open: false,
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
        self.model.read(cx).busy_reason(&self.id, cx)
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
        let java = config.java_override.clone().unwrap_or_default();
        if !self.has_pending_saves() {
            if self.requested_name.as_ref() == Some(&config.name) {
                self.requested_name = None;
            }
            if self.requested_window == Some(config.window) {
                self.requested_window = None;
            }
            if self.requested_java.as_ref() == Some(&java.java_path) {
                self.requested_java = None;
            }
            if self.requested_gc == Some(java.gc_preset) {
                self.requested_gc = None;
            }
            if self.requested_global == Some(config.java_override.is_none()) {
                self.requested_global = None;
                if config.java_override.is_none() {
                    self.requested_java = None;
                    self.requested_gc = None;
                    self.requested_memory = None;
                    self.requested_args = None;
                }
            }
            if self.requested_memory == Some((java.min_mem_mib, java.max_mem_mib)) {
                self.requested_memory = None;
            }
            if self.requested_args.as_ref() == Some(&java.extra_jvm_args) {
                self.requested_args = None;
            }
        }
        if !force && self.seen.as_ref() == Some(&config) {
            return;
        }
        self.seen = Some(config.clone());
        if self.requested_name.is_none() {
            Self::set_text(&self.name, &config.name, force, window, cx);
        }
        if self.requested_memory.is_none() {
            Self::set_text(
                &self.min_mem,
                &java.min_mem_mib.to_string(),
                force,
                window,
                cx,
            );
            let max = java.max_mem_mib.map(|m| m.to_string()).unwrap_or_default();
            Self::set_text(&self.max_mem, &max, force, window, cx);
        }
        if self.requested_window.is_none() {
            let size = config
                .window
                .map_or(DEFAULT_WINDOW, |w| (w.width, w.height));
            Self::set_text(&self.width, &size.0.to_string(), force, window, cx);
            Self::set_text(&self.height, &size.1.to_string(), force, window, cx);
        }
        if self.requested_args.is_none()
            && self.args.read(cx).value() != java.extra_jvm_args.as_str()
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

    fn has_pending_saves(&self) -> bool {
        self.saving || !self.queue.is_empty() || self.mode_seed_pending
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
        if self.has_pending_saves() {
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
        if self.has_pending_saves() {
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
        let choice = self.java_choice_revision;
        let probe = Core::global(cx).probe_java.clone();
        let job = run_blocking(cx, {
            let path = path.clone();
            move || {
                probe(&path).map(|i| {
                    let vendor = crate::java_settings::vendor_name(&i.path);
                    (i, vendor)
                })
            }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let install = job.await.ok().flatten();
            this.update_in(cx, |this, window, cx| {
                if this.java_choice_revision != choice {
                    this.probe_started.remove(&path);
                    return;
                }
                let major = install.as_ref().map(|(i, _)| i.major);
                if let Some((install, vendor)) = install {
                    this.java_list.update(cx, |list, cx| {
                        list.remember_probe(path.clone(), install, vendor, cx)
                    });
                }
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
        installs
            .iter()
            .find(|i| list.matches(Some(&i.path), Some(path)))
            .map(|i| i.major)
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
        if self.saving || self.mode_seed_pending {
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
                    if !this.queue.iter().any(|(queued, _)| *queued == field) {
                        match field {
                            Field::Memory => this.requested_memory = None,
                            Field::Args => this.requested_args = None,
                            Field::Name => this.requested_name = None,
                            Field::Window => this.requested_window = None,
                            Field::Java => this.requested_java = None,
                            Field::Gc => this.requested_gc = None,
                            Field::Global => this.requested_global = None,
                        }
                    }
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

    /// Changes the saved config with `change`, read fresh from disk under
    /// the config lock (`InstanceStore::update`), so a loader operation's
    /// commit in between is not overwritten.
    fn update_config(
        &mut self,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut InstanceConfig) + Send + 'static,
    ) {
        self.save(field, window, cx, move |store, id| {
            store.update(id, change).map(drop)
        });
    }

    fn commit_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.config(cx) else {
            return;
        };
        let text = self.name.read(cx).value().trim().to_string();
        if self.requested_name.as_ref().unwrap_or(&config.name) == &text {
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
        self.requested_name = Some(text.clone());
        self.save(Field::Name, window, cx, move |store, id| {
            store.rename(id, &text).map(drop)
        });
    }

    /// Reads the memory inputs: the minimum, and the maximum (`None` for
    /// automatic), or the reason they are not acceptable.
    fn read_memory(&self, cx: &App) -> Result<(u32, Option<u32>), String> {
        parse_memory(
            &self.min_mem.read(cx).value(),
            &self.max_mem.read(cx).value(),
        )
    }

    fn commit_memory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.config(cx) else {
            return;
        };
        // Nothing to commit while the global settings apply.
        if self
            .requested_global
            .unwrap_or(config.java_override.is_none())
        {
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
        if self
            .requested_memory
            .or_else(|| saved.map(|j| (j.min_mem_mib, j.max_mem_mib)))
            == Some((min, max))
        {
            return;
        }
        self.requested_memory = Some((min, max));
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
        if self
            .requested_global
            .unwrap_or(config.java_override.is_none())
        {
            return;
        }
        let text = self.args.read(cx).value().to_string();
        if validate_extra_jvm_args(&text).is_err() {
            self.args_changed(cx);
            return;
        }
        let saved = config.java_override.as_ref();
        if self
            .requested_args
            .as_deref()
            .or_else(|| saved.map(|j| j.extra_jvm_args.as_str()))
            == Some(text.as_str())
        {
            return;
        }
        self.requested_args = Some(text.clone());
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
        self.requested_gc = Some(preset);
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
        self.java_choice_revision += 1;
        self.java_file_pending = None;
        self.requested_java = Some(path.clone());
        // The newer Java job supersedes an older incompatible GC request.
        // Clear at acceptance, not completion, so a later explicit GC survives.
        if self
            .requested_gc
            .is_some_and(|preset| major.is_some_and(|major| !preset.is_compatible(major)))
        {
            self.requested_gc = None;
        }
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
        self.java_choice_revision += 1;
        let choice = self.java_choice_revision;
        self.java_file_pending = None;
        cx.notify();
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
                this.update_in(cx, |this, window, cx| {
                    if this.java_choice_revision == choice {
                        this.use_java_file(path, window, cx).detach();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Checks that `path` runs as a Java, then uses it. Not a Java: says
    /// so and saves nothing.
    pub fn use_java_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        self.java_choice_revision += 1;
        let choice = self.java_choice_revision;
        self.java_file_pending = Some(path.clone());
        self.errors.java = None;
        cx.notify();
        let probe = Core::global(cx).probe_java.clone();
        let job = run_blocking(cx, {
            let path = path.clone();
            move || {
                probe(&path).map(|i| {
                    let vendor = crate::java_settings::vendor_name(&i.path);
                    (i, vendor)
                })
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update_in(cx, |this, window, cx| {
                if this.java_choice_revision != choice {
                    return;
                }
                this.java_file_pending = None;
                match result {
                    Ok(Some((install, vendor))) => {
                        this.java_list.update(cx, |list, cx| {
                            list.remember_probe(path.clone(), install.clone(), vendor.clone(), cx)
                        });
                        this.probed.insert(path.clone(), Some(install.major));
                        this.set_java(Some(path), Some(install.major), window, cx);
                    }
                    _ => {
                        this.errors.java =
                            Some(t!("instance_settings.java_not_java").to_string().into());
                        cx.notify();
                    }
                }
            })
            .ok();
        })
    }

    /// Turns "使用全局设置" on (no override; the override is kept for
    /// turning it off again) or off (the kept override, else one seeded
    /// from the global config).
    fn set_use_global(&mut self, global: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.java_choice_revision += 1;
        self.java_file_pending = None;
        let revision = self.mode_revision.start();
        // What was typed belongs to the override that is going away.
        if global {
            drop(self.flush(window, cx));
            self.requested_global = Some(true);
            let kept = self.discarded.clone();
            self.update_config(Field::Global, window, cx, move |config| {
                if let Some(old) = config.java_override.take() {
                    *kept.lock().unwrap_or_else(|e| e.into_inner()) = Some(old);
                }
            });
        } else {
            self.requested_global = Some(false);
            self.mode_seed_pending = true;
            let seed = Arc::new(Mutex::new(JavaSettings::default()));
            self.pending_restore_seeds.push(seed.clone());
            let kept = self.discarded.clone();
            // Enqueue the dependency now, before accepting any edits against it.
            // Later mode choices may supersede its seed, never its queue position.
            self.update_config(Field::Global, window, cx, move |config| {
                if config.java_override.is_none() {
                    let old = kept.lock().unwrap_or_else(|e| e.into_inner()).take();
                    config.java_override =
                        Some(old.unwrap_or_else(|| {
                            seed.lock().unwrap_or_else(|e| e.into_inner()).clone()
                        }));
                }
            });
            cx.notify();
        }
        if !self.mode_seed_pending {
            return;
        }
        // Even a final global-on choice must resolve earlier restorations: their
        // accepted edits must run before that choice retains/removes the override.
        let config = Core::read_config(cx);
        cx.spawn_in(window, async move |this, cx| {
            let seed = config.await.map(|c| c.java).unwrap_or_default();
            this.update_in(cx, |this, window, cx| {
                if !this.mode_revision.is_current(revision) {
                    return;
                }
                for pending in this.pending_restore_seeds.drain(..) {
                    *pending.lock().unwrap_or_else(|e| e.into_inner()) = seed.clone();
                }
                this.mode_seed_pending = false;
                this.pump(window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn set_window_mode(&mut self, custom: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.errors.window = None;
        if !custom {
            self.requested_window = Some(None);
            self.update_config(Field::Window, window, cx, |config| config.window = None);
            return;
        }
        let size = self.read_window(cx).unwrap_or(WindowSize {
            width: DEFAULT_WINDOW.0,
            height: DEFAULT_WINDOW.1,
        });
        self.requested_window = Some(Some(size));
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
        let Some(saved) = self.requested_window.unwrap_or(config.window) else {
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
            self.requested_window = Some(Some(size));
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
        let job = model.update(cx, |m, cx| m.delete(&self.id, cx));
        cx.spawn_in(window, async move |this, cx| {
            match job.await {
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

    pub(super) fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.deleting || self.busy_reason(cx).is_some() {
            return;
        }
        let name = self
            .model
            .read(cx)
            .display_name(&self.id)
            .unwrap_or_else(|| self.id.clone());
        let form = cx.entity();
        let footer_focus = cx.focus_handle();
        let first_draw = std::cell::Cell::new(true);
        window.open_alert_dialog(cx, move |alert, window, cx| {
            crate::content::focus_first_button(&footer_focus, &first_draw, window, cx);
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
                .footer(
                    div().w_full().track_focus(&footer_focus).child(
                        DialogFooter::new()
                            .child(
                                Button::new("cancel")
                                    .label(t!("instance_settings.delete_cancel").to_string())
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(
                                Button::new("ok")
                                    .danger()
                                    .label(t!("instance_settings.delete_confirm").to_string())
                                    .on_click(move |_, window, cx| {
                                        window.close_dialog(cx);
                                        form.update(cx, |form, cx| form.delete(window, cx));
                                    }),
                            ),
                    ),
                )
                .on_ok(|_, _, cx| {
                    cx.propagate();
                    false
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

pub(crate) fn java_options(
    list: &JavaList,
    current: Option<&PathBuf>,
) -> Vec<crate::java_settings::JavaOption> {
    use crate::java_settings::{JavaOption, JavaSource};
    let mut options = vec![JavaOption {
        path: None,
        vendor: None,
        version: String::new(),
        source: JavaSource::Automatic,
        selectable: true,
    }];
    options.extend(list.installs().unwrap_or_default().iter().map(|install| {
        list.presentation
            .get(&install.path)
            .cloned()
            .unwrap_or_else(|| JavaOption {
                path: Some(install.path.clone()),
                vendor: None,
                version: install.version.clone(),
                source: if list
                    .system_installs()
                    .unwrap_or_default()
                    .iter()
                    .any(|i| i.path == install.path)
                {
                    JavaSource::System
                } else {
                    JavaSource::Manual
                },
                selectable: true,
            })
    }));
    if let Some(path) = current
        && !options
            .iter()
            .any(|o| list.matches(o.path.as_ref(), Some(path)))
    {
        options.push(
            list.presentation
                .get(list.canonical_path(path))
                .cloned()
                .unwrap_or_else(|| JavaOption {
                    path: Some(path.clone()),
                    vendor: None,
                    version: String::new(),
                    source: JavaSource::Manual,
                    selectable: true,
                }),
        );
    }
    options
}

impl SettingsForm {
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
        let global = self
            .requested_global
            .unwrap_or(config.java_override.is_none());
        let section = Self::section(
            "settings-java-memory",
            t!("instance_settings.java_memory").to_string(),
            cx,
        )
        .child(
            v_flex().gap_1().child(
                Switch::new("settings-use-global")
                    .checked(global)
                    .label(t!("instance_settings.use_global").to_string())
                    .on_click(cx.listener(|this, checked: &bool, window, cx| {
                        this.set_use_global(*checked, window, cx);
                    })),
            ),
        );
        let Some(java) = config.java_override.as_ref().filter(|_| !global) else {
            return section;
        };
        section
            .child(self.render_java_choice(java, cx))
            .child(
                v_flex()
                    .gap_3()
                    .mt_3()
                    .child(crate::java_settings::group_heading(
                        gpui_kit::assets::IconName::MemoryStick,
                        t!("java_settings.memory").to_string(),
                        cx,
                    ))
                    .child(self.render_memory(cx)),
            )
            .child(
                v_flex()
                    .gap_3()
                    .mt_3()
                    .child(self.render_gc(java, cx))
                    .child(self.render_args(cx)),
            )
    }

    fn render_java_choice(
        &self,
        java: &JavaSettings,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let chosen = self.requested_java.as_ref().unwrap_or(&java.java_path);
        let current = self.java_file_pending.as_ref().or(chosen.as_ref());
        let options = java_options(self.java_list.read(cx), current);
        let selected = options
            .iter()
            .position(|o| self.java_list.read(cx).matches(o.path.as_ref(), current));
        let detecting = self.java_list.read(cx).installs.is_none();
        let radios: Vec<Radio> = options
            .iter()
            .map(|o| crate::java_settings::runtime_radio(o, None, cx))
            .collect();
        let paths: Vec<Option<PathBuf>> = options.into_iter().map(|o| o.path).collect();
        crate::java_settings::field(
            t!("instance_settings.java").to_string(),
            Some(gpui_kit::assets::IconName::Cpu),
            cx,
        )
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
        .when(self.java_file_pending.is_some(), |field| {
            field.child(Self::hint(
                "settings-java-checking",
                t!("instance_settings.java_checking").to_string(),
                cx,
            ))
        })
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
                        crate::java_settings::field(
                            t!("instance_settings.min_mem").to_string(),
                            None,
                            cx,
                        )
                        .child(
                            Input::new(&self.min_mem)
                                .id("settings-min-mem")
                                .w(rems(NUMBER_W.0)),
                        ),
                    )
                    .child(
                        crate::java_settings::field(
                            t!("instance_settings.max_mem").to_string(),
                            None,
                            cx,
                        )
                        .child(
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
        let mut effective = java.clone();
        if let Some(path) = &self.requested_java {
            effective.java_path = path.clone();
        }
        let disclosure_major = self.java_major(&effective, cx);
        let unknown = self.java_unknown(java, cx);
        // Without a known Java only the presets that need none are open.
        let compatible = |preset: GcPreset| gc_selectable(preset, major);
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
        crate::java_settings::field(
            t!("instance_settings.gc").to_string(),
            Some(gpui_kit::assets::IconName::SlidersHorizontal),
            cx,
        )
        .child(
            RadioGroup::new("settings-gc")
                .selected_index(
                    GC_PRESETS
                        .iter()
                        .position(|p| *p == self.requested_gc.unwrap_or(java.gc_preset)),
                )
                .children(radios)
                .on_click(cx.listener(|this, ix: &usize, window, cx| {
                    if let Some(preset) = GC_PRESETS.get(*ix) {
                        this.set_gc(*preset, window, cx);
                    }
                })),
        )
        .child(crate::java_settings::gc_disclosure(
            "settings-gc-details",
            self.requested_gc.unwrap_or(java.gc_preset),
            disclosure_major,
            self.gc_details_open,
            Button::new("settings-gc-details-toggle")
                .ghost()
                .small()
                .icon(if self.gc_details_open {
                    gpui_kit::assets::IconName::ChevronDown
                } else {
                    gpui_kit::assets::IconName::ChevronRight
                })
                .label(t!("java_settings.gc_details").to_string())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.gc_details_open = !this.gc_details_open;
                    cx.notify();
                })),
            cx,
        ))
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
        crate::java_settings::field(t!("instance_settings.jvm_args").to_string(), None, cx)
            .child(
                crate::fields::tab_leaves(div().id("settings-args").test_support())
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
        let custom = self.requested_window.unwrap_or(config.window).is_some();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_bridge::testing::{self, FakeCatalog};
    use gpui_kit::TestAppContext;
    use patina_core::paths::DataDirs;

    #[gpui_kit::test]
    fn java_refresh_requested_during_scan_is_not_dropped(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let rt = crate::runtime::start();
        let tmp = tempfile::tempdir().unwrap();
        let mut core = testing::core(
            rt.handle().clone(),
            DataDirs::new(tmp.path()),
            Arc::new(FakeCatalog::new()),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (started, wait_started) = std::sync::mpsc::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        let wait_release = Mutex::new(wait_release);
        core.detect_java = Arc::new(move || {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                started.send(()).unwrap();
                wait_release.lock().unwrap().recv().unwrap();
            }
            Vec::new()
        });
        cx.executor().allow_parking();
        let list = cx.update(|cx| {
            cx.set_global(core);
            let list = java_list(cx);
            redetect_java(&list, cx);
            list
        });
        wait_started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        cx.update(|cx| redetect_java(&list, cx));
        release.send(()).unwrap();
        for _ in 0..100 {
            cx.run_until_parked();
            if calls.load(Ordering::SeqCst) >= 2 && !list.read_with(cx, |l, _| l.is_loading()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "download/delete refresh must be retried after the current scan completes"
        );
    }

    struct Host {
        model: Entity<InstancesModel>,
    }
    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui_kit::test]
    async fn completed_write_with_stale_listing_keeps_the_reverted_intent(cx: &mut TestAppContext) {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = testing::core(
            rt.handle().clone(),
            DataDirs::new(dir.path()),
            Arc::new(FakeCatalog::new().with_manifest_error("offline")),
        );
        let store = core.store.clone();
        let inst = store.create("Alpha", "1.21.1").unwrap();
        store
            .update(&inst.id, |c| {
                c.java_override = Some(JavaSettings::default())
            })
            .unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(core);
        });
        cx.executor().allow_parking();
        let window = cx.add_window(|window, cx| Host {
            model: cx.new(|cx| InstancesModel::new(window, cx)),
        });
        let model = window.read_with(cx, |h, _| h.model.clone()).unwrap();
        model.update(cx, |m, cx| m.refresh_and_wait(cx)).await;
        let form = cx
            .update_window(window.into(), |_, window, cx| {
                cx.new(|cx| SettingsForm::new(model.clone(), inst.id.clone(), window, cx))
            })
            .unwrap();
        // Hold the pump, commit through the real methods, and complete the
        // captured production jobs without delivering their listing yet.
        let jobs = cx
            .update_window(window.into(), |_, window, cx| {
                form.update(cx, |f, cx| {
                    f.saving = true;
                    set_inputs_for_test(f, "768", "-Dpending=true", window, cx);
                    f.commit_memory(window, cx);
                    f.commit_args(window, cx);
                    f.queue.drain(..).collect::<Vec<_>>()
                })
            })
            .unwrap();
        assert_eq!(jobs.len(), 2);
        for (_, job) in jobs {
            let store = store.clone();
            let id = inst.id.clone();
            cx.update(|cx| run_blocking(cx, move || job(&store, &id)))
                .await
                .unwrap()
                .unwrap();
        }
        form.update(cx, |f, _| f.saving = false);
        assert_eq!(
            form.read_with(cx, |f, cx| f
                .config(cx)
                .unwrap()
                .java_override
                .unwrap()
                .min_mem_mib),
            512
        );
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .open(inst.dir.join(patina_core::instance::CONFIG_LOCK_FILE))
            .unwrap();
        lock.lock().unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            form.update(cx, |f, cx| {
                set_inputs_for_test(f, "512", "", window, cx);
                f.commit_memory(window, cx);
                f.commit_args(window, cx);
                assert_eq!(f.requested_memory, Some((512, None)));
                assert_eq!(f.requested_args.as_deref(), Some(""));
            })
        })
        .unwrap();
        // Deliver the older readback (768) while the latest request is 512.
        model.update(cx, |m, cx| m.refresh_and_wait(cx)).await;
        cx.update_window(window.into(), |_, window, cx| {
            form.update(cx, |f, cx| {
                f.apply_config(window, cx, false);
                assert_eq!(f.min_mem.read(cx).value().as_ref(), "512");
                assert!(f.args.read(cx).value().is_empty());
            })
        })
        .unwrap();
        drop(lock);
        let drained = cx
            .update_window(window.into(), |_, window, cx| {
                form.update(cx, |f, cx| f.flush(window, cx))
            })
            .unwrap();
        drained.await;
        model.update(cx, |m, cx| m.refresh_and_wait(cx)).await;
        cx.update_window(window.into(), |_, window, cx| {
            form.update(cx, |f, cx| {
                f.apply_config(window, cx, false);
                assert!(f.requested_memory.is_none());
                assert!(f.requested_args.is_none());
            })
        })
        .unwrap();
        let saved = store.load(&inst.id).unwrap().config.java_override.unwrap();
        assert_eq!(saved.min_mem_mib, 512);
        assert!(saved.extra_jvm_args.is_empty());
    }

    #[gpui_kit::test]
    async fn java_gc_reset_supersedes_older_intent_without_intermediate_readback(
        cx: &mut TestAppContext,
    ) {
        gc_reset_with_held_saves(cx, false).await;
    }

    #[gpui_kit::test]
    async fn java_gc_reset_preserves_a_newer_explicit_preset(cx: &mut TestAppContext) {
        gc_reset_with_held_saves(cx, true).await;
    }

    async fn gc_reset_with_held_saves(cx: &mut TestAppContext, newer_choice: bool) {
        use gpui_kit::test::TestWindowExt;
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = testing::core(
            rt.handle().clone(),
            DataDirs::new(dir.path()),
            Arc::new(FakeCatalog::new().with_manifest_error("offline")),
        );
        let store = core.store.clone();
        let inst = store.create("Alpha", "1.21.1").unwrap();
        store
            .update(&inst.id, |c| {
                c.java_override = Some(JavaSettings {
                    java_path: Some("/opt/j21/bin/java".into()),
                    ..Default::default()
                })
            })
            .unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(core);
            seed_java_list(
                vec![
                    JavaInstall {
                        path: "/opt/j21/bin/java".into(),
                        major: 21,
                        version: "21.0.1".into(),
                        is_64bit: true,
                    },
                    JavaInstall {
                        path: "/opt/j8/bin/java".into(),
                        major: 8,
                        version: "8.0.1".into(),
                        is_64bit: true,
                    },
                ],
                cx,
            );
        });
        cx.executor().allow_parking();
        let host = cx.add_window(|window, cx| Host {
            model: cx.new(|cx| InstancesModel::new(window, cx)),
        });
        let model = host.read_with(cx, |h, _| h.model.clone()).unwrap();
        model.update(cx, |m, cx| m.refresh_and_wait(cx)).await;
        let window = cx.add_window(|window, cx| {
            crate::theme::apply(window, cx);
            SettingsForm::new(model.clone(), inst.id.clone(), window, cx)
        });
        let form = window.root(cx).unwrap();
        cx.simulate_window_resize(
            window.into(),
            gpui_kit::size(gpui_kit::px(1100.), gpui_kit::px(1600.)),
        );
        // Capture actual production save jobs at the pump boundary. No intermediate
        // listing is delivered, just as a replaced model refresh may be omitted.
        form.update(cx, |f, _| f.saving = true);
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.within("settings-gc").click(2usize, cx);
            assert_eq!(
                window.within("settings-gc").find(2usize).checked(),
                Some(true)
            );
            window.within("settings-java").click(2usize, cx);
            if newer_choice {
                window.within("settings-gc").click(1usize, cx);
            }
        })
        .unwrap();
        let jobs = form.update(cx, |f, _| f.queue.drain(..).collect::<Vec<_>>());
        assert_eq!(jobs.len(), if newer_choice { 3 } else { 2 });
        for (_, job) in jobs {
            let store = store.clone();
            let id = inst.id.clone();
            cx.update(|cx| run_blocking(cx, move || job(&store, &id)))
                .await
                .unwrap()
                .unwrap();
        }
        form.update(cx, |f, _| f.saving = false);
        model.update(cx, |m, cx| m.refresh_and_wait(cx)).await;
        let java = store.load(&inst.id).unwrap().config.java_override.unwrap();
        assert_eq!(java.java_path, Some("/opt/j8/bin/java".into()));
        assert_eq!(
            java.gc_preset,
            if newer_choice {
                GcPreset::G1Aikar
            } else {
                GcPreset::None
            }
        );
        cx.update_window(window.into(), |_, window, cx| {
            form.update(cx, |f, cx| f.apply_config(window, cx, false));
            window.render_frame(cx);
            let expected = if newer_choice { 1usize } else { 0usize };
            assert_eq!(
                window.within("settings-gc").find(expected).checked(),
                Some(true)
            );
            assert_ne!(
                window.within("settings-gc").find(2usize).checked(),
                Some(true)
            );
            assert_eq!(
                window.within("settings-java").find(2usize).checked(),
                Some(true)
            );
        })
        .unwrap();
    }

    fn set_inputs_for_test(
        f: &mut SettingsForm,
        memory: &str,
        args: &str,
        window: &mut Window,
        cx: &mut Context<SettingsForm>,
    ) {
        SettingsForm::set_text(&f.min_mem, memory, true, window, cx);
        f.args
            .update(cx, |input, cx| input.set_value(args, window, cx));
    }
}
