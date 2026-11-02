//! Java 与内存: the defaults every instance without its own settings uses
//! (memory, GC preset, JVM arguments; the instance settings' rules), the
//! managed runtimes (deletable, after asking) and the Java found on this
//! computer (read only).

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{Disableable, Sizable, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, Focusable, InteractiveElement, IntoElement,
    ParentElement, Rems, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window,
    div, rems,
};
use patina_core::jvm::{GcPreset, validate_extra_jvm_args};
use rust_i18n::t;

use super::{Field, Section, SettingsPage};
use crate::content::focus_first_button;
use crate::core_bridge::{ConfigChanged, Core};
use crate::instances::{GC_PRESETS, parse_memory, redetect_java};

/// Work to do in a window (a dialog's commit).
type WindowAction = dyn Fn(&mut Window, &mut App);

/// Width of the numeric inputs.
const NUMBER_W: Rems = Rems(9.);

/// Element id of managed runtime `component`'s row.
pub fn runtime_row_id(component: &str) -> ElementId {
    ElementId::Name(format!("prefs-runtime-{component}").into())
}

/// Element id of managed runtime `component`'s "删除…".
pub fn runtime_delete_id(component: &str) -> ElementId {
    ElementId::Name(format!("prefs-runtime-delete-{component}").into())
}

/// Puts `text` in `input` unless it holds it already or the user is in it.
fn set_text(
    input: &Entity<gpui_kit::component::input::InputState>,
    text: &str,
    window: &mut Window,
    cx: &mut App,
) {
    if input.read(cx).value() == text || input.focus_handle(cx).is_focused(window) {
        return;
    }
    input.update(cx, |input, cx| {
        input.set_value(text.to_string(), window, cx)
    });
}

impl SettingsPage {
    /// Sets the fields from the config as last read, leaving the one being
    /// typed in alone.
    pub(super) fn apply_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(java) = self.config.as_ref().map(|c| c.java.clone()) else {
            return;
        };
        if let Some(path) = java.java_path.clone() {
            self.probe_saved_java(path, cx);
        }
        self.inputs_ready = true;
        if self.requested_memory.is_none() {
            set_text(&self.min_mem, &java.min_mem_mib.to_string(), window, cx);
            let max = java.max_mem_mib.map(|m| m.to_string()).unwrap_or_default();
            set_text(&self.max_mem, &max, window, cx);
        }
        if self.requested_args.is_none()
            && self.args.read(cx).value() != java.extra_jvm_args.as_str()
            && !self.args.focus_handle(cx).is_focused(window)
        {
            self.args.update(cx, |args, cx| {
                args.set_value(java.extra_jvm_args.clone(), window, cx)
            });
            self.errors.args = None;
        }
        cx.notify();
    }

    pub(super) fn commit_memory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.config.as_ref().map(|c| c.java.clone()) else {
            return;
        };
        let (min, max) = (self.min_mem.read(cx).value(), self.max_mem.read(cx).value());
        let (min, max) = match parse_memory(&min, &max) {
            Ok(memory) => memory,
            Err(reason) => {
                self.errors.memory = Some(reason.into());
                cx.notify();
                return;
            }
        };
        self.errors.memory = None;
        cx.notify();
        if self
            .requested_memory
            .unwrap_or((saved.min_mem_mib, saved.max_mem_mib))
            == (min, max)
        {
            return;
        }
        self.requested_memory = Some((min, max));
        self.edit(Field::Memory, window, cx, move |c| {
            c.java.min_mem_mib = min;
            c.java.max_mem_mib = max;
        });
    }

    /// "自动" for the maximum.
    fn max_auto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.max_mem
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.commit_memory(window, cx);
    }

    pub(super) fn args_changed(&mut self, cx: &mut Context<Self>) {
        let text = self.args.read(cx).value();
        self.errors.args = validate_extra_jvm_args(&text)
            .err()
            .map(|error| error.to_string().into());
        cx.notify();
    }

    pub(super) fn commit_args(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.config.as_ref().map(|c| c.java.extra_jvm_args.clone()) else {
            return;
        };
        let text = self.args.read(cx).value().to_string();
        if validate_extra_jvm_args(&text).is_err() {
            self.args_changed(cx);
            return;
        }
        if self.requested_args.as_ref().unwrap_or(&saved) == &text {
            return;
        }
        self.requested_args = Some(text.clone());
        self.edit(Field::Args, window, cx, move |c| {
            c.java.extra_jvm_args = text
        });
    }

    fn set_gc(&mut self, preset: GcPreset, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .requested_gc
            .or_else(|| self.config.as_ref().map(|c| c.java.gc_preset))
            == Some(preset)
        {
            return;
        }
        self.requested_gc = Some(preset);
        self.edit(Field::Gc, window, cx, move |c| c.java.gc_preset = preset);
    }

    /// Saves a detected executable or restores automatic managed Java.
    pub(super) fn set_java(
        &mut self,
        path: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.java_choice_revision += 1;
        self.java_file_pending = None;
        self.errors.java = None;
        self.requested_java = Some(path.clone());
        self.edit(Field::Java, window, cx, move |c| c.java.java_path = path);
    }

    fn choose_java_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.java_choice_revision += 1;
        let revision = self.java_choice_revision;
        self.java_file_pending = None;
        self.pump(window, cx);
        cx.notify();
        let chosen = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
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
                    if this.java_choice_revision == revision {
                        this.use_java_file(path, window, cx).detach();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Validates a manually chosen executable through the same probe as the
    /// instance selector. A newer choice supersedes an unfinished probe.
    pub fn use_java_file(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Task<()> {
        self.java_choice_revision += 1;
        let revision = self.java_choice_revision;
        self.java_file_pending = Some(path.clone());
        self.errors.java = None;
        cx.notify();
        let probe = Core::global(cx).probe_java.clone();
        let job = crate::core_bridge::run_blocking(cx, {
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
                if this.java_choice_revision != revision {
                    return;
                }
                this.java_file_pending = None;
                match result {
                    Ok(Some((install, vendor))) => {
                        this.java_list.update(cx, |list, cx| {
                            list.remember_probe(path.clone(), install.clone(), vendor.clone(), cx)
                        });
                        this.probed_java.insert(path.clone(), Some(install.major));
                        this.set_java(Some(path), window, cx);
                    }
                    _ => {
                        this.errors.java =
                            Some(t!("instance_settings.java_not_java").to_string().into());
                        this.pump(window, cx);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
    }

    /// Read a persisted manual Java's version for disclosure only; never
    /// change the chosen preset or launch compatibility policy here.
    fn probe_saved_java(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        if self.probed_java.contains_key(&path)
            || self
                .java_list
                .read(cx)
                .installs()
                .is_some_and(|list| list.iter().any(|i| i.path == path))
        {
            return;
        }
        self.probed_java.insert(path.clone(), None);
        let probe = Core::global(cx).probe_java.clone();
        let job = crate::core_bridge::run_blocking(cx, {
            let path = path.clone();
            move || {
                probe(&path).map(|i| {
                    let vendor = crate::java_settings::vendor_name(&i.path);
                    (i, vendor)
                })
            }
        });
        cx.spawn(async move |this, cx| {
            let install = job.await.ok().flatten();
            this.update(cx, |this, cx| {
                let major = install.as_ref().map(|(i, _)| i.major);
                if let Some((install, vendor)) = install {
                    this.java_list.update(cx, |list, cx| {
                        list.remember_probe(path.clone(), install, vendor, cx)
                    });
                }
                this.probed_java.insert(path, major);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn java_major(&self, cx: &App) -> Option<u32> {
        let path = self
            .requested_java
            .as_ref()
            .or_else(|| self.config.as_ref().map(|c| &c.java.java_path))?
            .as_ref()?;
        self.java_list
            .read(cx)
            .installs()
            .unwrap_or_default()
            .iter()
            .find(|i| self.java_list.read(cx).matches(Some(&i.path), Some(path)))
            .map(|i| i.major)
            .or_else(|| self.probed_java.get(path).copied().flatten())
    }

    fn set_low_memory_warning(
        &mut self,
        checked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.requested_low_memory_warning = Some(checked);
        self.edit(Field::LowMemoryWarning, window, cx, move |c| {
            c.java.low_mem_warning = checked
        });
    }

    fn render_java_choice(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let chosen = self
            .requested_java
            .as_ref()
            .or_else(|| self.config.as_ref().map(|c| &c.java.java_path))
            .and_then(|p| p.as_ref());
        let current = self.java_file_pending.as_ref().or(chosen);
        let options = crate::instances::java_options(self.java_list.read(cx), current);
        let selected = options
            .iter()
            .position(|o| self.java_list.read(cx).matches(o.path.as_ref(), current));
        let radios: Vec<Radio> = options
            .iter()
            .map(|o| crate::java_settings::runtime_radio(o, self.runtime_menu(o, cx), cx))
            .collect();
        let paths: Vec<_> = options.into_iter().map(|o| o.path).collect();
        crate::java_settings::field(
            t!("instance_settings.java").to_string(),
            Some(IconName::Cpu),
            cx,
        )
        .child(
            RadioGroup::new("prefs-java-choice")
                .selected_index(selected)
                .disabled(self.config.is_none())
                .children(radios)
                .on_click(cx.listener(move |this, ix: &usize, window, cx| {
                    if let Some(path) = paths.get(*ix) {
                        this.set_java(path.clone(), window, cx);
                    }
                })),
        )
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    Button::new("prefs-java-download")
                        .outline()
                        .small()
                        .label(t!("java_download.open").to_string())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_java_download(None, window, cx)
                        })),
                )
                .child(
                    Button::new("prefs-redetect")
                        .outline()
                        .small()
                        .icon(IconName::RefreshCw)
                        .label(t!("prefs.redetect").to_string())
                        .disabled(self.java_list.read(cx).is_loading())
                        .loading(self.java_list.read(cx).is_loading())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.reload_runtimes(cx);
                            redetect_java(&this.java_list, cx);
                        })),
                )
                .child(
                    Button::new("prefs-java-choose")
                        .outline()
                        .small()
                        .label(t!("instance_settings.java_choose").to_string())
                        .disabled(self.config.is_none())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.choose_java_file(window, cx)),
                        ),
                ),
        )
        .when(self.runtimes_locked(cx), |f| {
            f.child(Self::hint(
                "prefs-runtimes-busy",
                t!("prefs.runtimes_busy").to_string(),
                cx,
            ))
        })
        .when(self.java_file_pending.is_some(), |f| {
            f.child(Self::hint(
                "prefs-java-checking",
                t!("instance_settings.java_checking").to_string(),
                cx,
            ))
        })
        .children(Self::error(
            "prefs-java-choice-error",
            self.errors.java.as_ref(),
            cx,
        ))
    }

    fn render_low_memory_warning(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .gap_1()
            .child(
                gpui_kit::component::switch::Switch::new("prefs-low-memory-warning")
                    .label(t!("java_settings.low_memory_warning").to_string())
                    .checked(
                        self.requested_low_memory_warning
                            .or_else(|| self.config.as_ref().map(|c| c.java.low_mem_warning))
                            .unwrap_or(true),
                    )
                    .disabled(self.config.is_none())
                    .on_click(cx.listener(|this, checked: &bool, window, cx| {
                        this.set_low_memory_warning(*checked, window, cx)
                    })),
            )
            .children(Self::error(
                "prefs-low-memory-warning-error",
                self.errors.low_memory_warning.as_ref(),
                cx,
            ))
    }

    // --- managed runtimes ---------------------------------------------------

    /// Whether runtimes may be deleted now: not while a game starts or
    /// runs, nor while a task (which may install one) is under way.
    fn runtimes_locked(&self, cx: &App) -> bool {
        let activity = self.model.read(cx).activity().read(cx);
        activity.has_work_in_flight() || activity.any_game_busy()
    }

    /// Asks before deleting managed runtime `component`: `删除"<名称>"？`,
    /// 删除 / 取消, with the keyboard on 取消.
    pub fn confirm_delete_runtime(
        &mut self,
        component: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.removing.contains(component) || self.runtimes_locked(cx) {
            return;
        }
        let this = cx.weak_entity();
        let component = component.to_string();
        let title = SharedString::from(t!("prefs.runtime_delete_title", name = component));
        // The footer's region: where the keyboard goes to reach 取消, and
        // how Enter tells whether a button has it.
        let footer_focus = cx.focus_handle();
        let first_draw = Cell::new(true);
        window.open_alert_dialog(cx, move |alert, window, cx| {
            focus_first_button(&footer_focus, &first_draw, window, cx);
            let delete: Rc<WindowAction> = {
                let (this, component) = (this.clone(), component.clone());
                Rc::new(move |window, cx| {
                    this.update(cx, |this, cx| this.delete_runtime(&component, window, cx))
                        .ok();
                })
            };
            let footer = DialogFooter::new()
                .child(
                    Button::new("prefs-runtime-cancel")
                        .label(t!("prefs.cancel").to_string())
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child({
                    let delete = delete.clone();
                    Button::new("prefs-runtime-ok")
                        .danger()
                        .label(t!("prefs.runtime_delete_confirm").to_string())
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            delete(window, cx);
                        })
                });
            let footer_focus = footer_focus.clone();
            let body = t!("prefs.runtime_delete_body").to_string();
            alert
                .title(
                    div()
                        .id("prefs-runtime-delete-title")
                        .aria_label(title.clone())
                        .test_support()
                        .child(title.clone()),
                )
                .description(
                    div()
                        .id("prefs-runtime-delete-body")
                        .aria_label(body.clone())
                        .test_support()
                        .child(body),
                )
                .footer(div().w_full().track_focus(&footer_focus).child(footer))
                // Enter deletes from the dialog itself; on a button it is
                // that button's own key, so on 取消 it cancels.
                .on_ok(move |_, window, cx| {
                    if footer_focus.contains_focused(window, cx) {
                        cx.propagate();
                        return false;
                    }
                    delete(window, cx);
                    true
                })
        });
    }

    /// Deletes managed runtime `component` (waiting for an install of it
    /// that is under way), then lists the runtimes again.
    fn delete_runtime(&mut self, component: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.runtimes_locked(cx) || !self.removing.insert(component.to_string()) {
            return;
        }
        let key = component.to_string();
        let removal = Core::remove_mojang_runtime(cx, key.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = removal.await;
            this.update_in(cx, |this, window, cx| {
                this.removing.remove(&key);
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(error) => Some(error.to_string()),
                };
                if let Some(error) = error {
                    tracing::warn!(%error, component = key, "deleting a runtime failed");
                    let text = t!("prefs.runtime_delete_failed", name = key, error = error);
                    window.push_notification(Notification::error(text.to_string()), cx);
                }
                this.reload_runtimes(cx);
                redetect_java(&this.java_list, cx);
                // The status bar counts them.
                ConfigChanged::bump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // --- drawing ------------------------------------------------------------

    pub(super) fn render_java(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        Self::section(Section::Java, cx)
            .child(self.render_java_choice(cx))
            .child(
                v_flex()
                    .gap_3()
                    .mt_3()
                    .child(crate::java_settings::group_heading(
                        IconName::MemoryStick,
                        t!("java_settings.memory").to_string(),
                        cx,
                    ))
                    .child(self.render_memory(cx))
                    .child(self.render_low_memory_warning(cx)),
            )
            .child(
                v_flex()
                    .gap_3()
                    .mt_3()
                    .child(self.render_gc(cx))
                    .child(self.render_args(cx)),
            )
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
                        .child(Input::new(&self.min_mem).id("prefs-min-mem").w(NUMBER_W)),
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
                                .child(Input::new(&self.max_mem).id("prefs-max-mem").w(NUMBER_W))
                                .child(
                                    Button::new("prefs-max-auto")
                                        .ghost()
                                        .small()
                                        .label(t!("instance_settings.max_auto").to_string())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.max_auto(window, cx)
                                        })),
                                ),
                        ),
                    ),
            )
            .children(Self::error(
                "prefs-memory-error",
                self.errors.memory.as_ref(),
                cx,
            ))
    }

    /// Every preset: the default applies to instances on different Java
    /// versions, and one whose Java cannot run the preset launches without
    /// it (with a warning), so the choice is not gated by any one Java.
    fn render_gc(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let preset = self
            .requested_gc
            .or_else(|| self.config.as_ref().map(|c| c.java.gc_preset));
        let radios: Vec<Radio> = GC_PRESETS
            .iter()
            .map(|p| Radio::new("gc").label(p.display_name()))
            .collect();
        crate::java_settings::field(
            t!("instance_settings.gc").to_string(),
            Some(IconName::SlidersHorizontal),
            cx,
        )
        .child(
            RadioGroup::new("prefs-gc")
                .selected_index(preset.and_then(|p| GC_PRESETS.iter().position(|g| *g == p)))
                .disabled(self.config.is_none())
                .children(radios)
                .on_click(cx.listener(|this, ix: &usize, window, cx| {
                    if let Some(preset) = GC_PRESETS.get(*ix) {
                        this.set_gc(*preset, window, cx);
                    }
                })),
        )
        .when(
            preset.is_some_and(|preset| preset.min_java_major() > 0),
            |field| {
                field.child(Self::hint(
                    "prefs-gc-note",
                    t!("prefs.gc_note").to_string(),
                    cx,
                ))
            },
        )
        .children(Self::error("prefs-gc-error", self.errors.gc.as_ref(), cx))
        .child(crate::java_settings::gc_disclosure(
            "prefs-gc-details",
            preset.unwrap_or_default(),
            self.java_major(cx),
            self.gc_details_open,
            Button::new("prefs-gc-details-toggle")
                .ghost()
                .small()
                .icon(if self.gc_details_open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .label(t!("java_settings.gc_details").to_string())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.gc_details_open = !this.gc_details_open;
                    cx.notify();
                })),
            cx,
        ))
    }

    fn render_args(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        crate::java_settings::field(t!("instance_settings.jvm_args").to_string(), None, cx)
            .child(
                crate::fields::tab_leaves(div().id("prefs-args").test_support())
                    .child(Textarea::new(&self.args).h(rems(5.5))),
            )
            .child(Self::hint(
                "prefs-args-hint",
                t!("instance_settings.jvm_args_hint").to_string(),
                cx,
            ))
            .children(Self::error(
                "prefs-args-error",
                self.errors.args.as_ref(),
                cx,
            ))
    }

    fn runtime_menu(
        &self,
        option: &crate::java_settings::JavaOption,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        use crate::java_settings::JavaSource;
        let this = cx.weak_entity();
        match &option.source {
            JavaSource::Vendor(id) => {
                let release = self
                    .distributions
                    .iter()
                    .find(|i| i.release().id() == *id)?
                    .release()
                    .clone();
                let id = id.clone();
                let removing = self.removing.contains(&id);
                let saving = self.is_saving();
                Some(
                    Button::new(SharedString::from(format!("java-actions-{id}")))
                        .ghost()
                        .small()
                        .icon(IconName::Ellipsis)
                        .accessibility_label(t!("java_list.actions").to_string())
                        .tooltip(t!("java_list.actions").to_string())
                        .dropdown_menu_with_anchor(gpui_kit::Anchor::TopRight, move |menu, _, _| {
                            let update = this.clone();
                            let delete = this.clone();
                            let release = release.clone();
                            let id = id.clone();
                            let menu = if saving {
                                menu.label(t!("java_list.saving").to_string())
                            } else {
                                menu
                            };
                            menu.item(
                                PopupMenuItem::new(t!("java_download.check_update").to_string())
                                    .on_click(move |_, window, cx| {
                                        update
                                            .update(cx, |this, cx| {
                                                this.open_java_download(
                                                    Some(release.clone()),
                                                    window,
                                                    cx,
                                                )
                                            })
                                            .ok();
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(t!("prefs.runtime_delete").to_string())
                                    .icon(IconName::Trash)
                                    .disabled(removing || saving)
                                    .on_click(move |_, window, cx| {
                                        delete
                                            .update(cx, |this, cx| {
                                                this.confirm_delete_distribution(
                                                    id.clone(),
                                                    window,
                                                    cx,
                                                )
                                            })
                                            .ok();
                                    }),
                            )
                        })
                        .into_any_element(),
                )
            }
            JavaSource::Mojang(component) => {
                let component = component.clone();
                let locked = self.runtimes_locked(cx) || self.is_saving();
                let removing = self.removing.contains(&component);
                Some(
                    Button::new(runtime_delete_id(&component))
                        .ghost()
                        .small()
                        .icon(IconName::Ellipsis)
                        .accessibility_label(t!("java_list.actions").to_string())
                        .tooltip(t!("java_list.actions").to_string())
                        .disabled(locked || removing)
                        .dropdown_menu_with_anchor(gpui_kit::Anchor::TopRight, move |menu, _, _| {
                            let this = this.clone();
                            let component = component.clone();
                            menu.item(
                                PopupMenuItem::new(t!("prefs.runtime_delete").to_string())
                                    .icon(IconName::Trash)
                                    .on_click(move |_, window, cx| {
                                        this.update(cx, |this, cx| {
                                            this.confirm_delete_runtime(&component, window, cx)
                                        })
                                        .ok();
                                    }),
                            )
                        })
                        .into_any_element(),
                )
            }
            _ => None,
        }
    }
}
