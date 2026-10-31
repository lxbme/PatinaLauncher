//! Java 与内存: the defaults every instance without its own settings uses
//! (memory, GC preset, JVM arguments; the instance settings' rules), the
//! managed runtimes (deletable, after asking) and the Java found on this
//! computer (read only).

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, Focusable, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Rems, SharedString, StatefulInteractiveElement, Styled,
    TestSupportExt, Window, div, px, rems,
};
use patina_core::jvm::{GcPreset, validate_extra_jvm_args};
use rust_i18n::t;

use super::{Field, Section, SettingsPage};
use crate::content::focus_first_button;
use crate::core_bridge::{ConfigChanged, Core, run};
use crate::fonts::Strong;
use crate::instances::{GC_PRESETS, parse_memory, redetect_java};
use crate::layout::TEXT_SECONDARY;

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
        set_text(&self.min_mem, &java.min_mem_mib.to_string(), window, cx);
        let max = java.max_mem_mib.map(|m| m.to_string()).unwrap_or_default();
        set_text(&self.max_mem, &max, window, cx);
        if self.args.read(cx).value() != java.extra_jvm_args.as_str()
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
        if saved.min_mem_mib == min && saved.max_mem_mib == max {
            return;
        }
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
        if saved == text {
            return;
        }
        self.edit(Field::Args, window, cx, move |c| {
            c.java.extra_jvm_args = text
        });
    }

    fn set_gc(&mut self, preset: GcPreset, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.as_ref().map(|c| c.java.gc_preset) == Some(preset) {
            return;
        }
        self.edit(Field::Gc, window, cx, move |c| c.java.gc_preset = preset);
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
        let root = Core::global(cx).data.runtimes();
        let key = component.to_string();
        let removal = {
            let key = key.clone();
            run(cx, async move {
                patina_core::java::ManagedRuntimes::new(root)
                    .remove(&key)
                    .await
            })
        };
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
            .child(Self::hint(
                "prefs-java-hint",
                t!("prefs.java_hint").to_string(),
                cx,
            ))
            .child(self.render_memory(cx))
            .child(self.render_gc(cx))
            .child(self.render_args(cx))
            .child(self.render_runtimes(cx))
            .child(self.render_detected(cx))
    }

    fn render_memory(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_4()
                    .items_end()
                    .child(
                        Self::field(t!("instance_settings.min_mem").to_string(), cx)
                            .child(Input::new(&self.min_mem).id("prefs-min-mem").w(NUMBER_W)),
                    )
                    .child(
                        Self::field(t!("instance_settings.max_mem").to_string(), cx).child(
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
        let preset = self.config.as_ref().map(|c| c.java.gc_preset);
        let radios: Vec<Radio> = GC_PRESETS
            .iter()
            .map(|p| Radio::new("gc").label(p.display_name()))
            .collect();
        Self::field(t!("instance_settings.gc").to_string(), cx)
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
            .child(Self::hint(
                "prefs-gc-note",
                t!("prefs.gc_note").to_string(),
                cx,
            ))
            .children(Self::error("prefs-gc-error", self.errors.gc.as_ref(), cx))
    }

    fn render_args(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        Self::field(t!("instance_settings.jvm_args").to_string(), cx)
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

    /// One two-line row: a title and a muted second line, then `action`.
    fn list_row(
        id: ElementId,
        title: String,
        detail: String,
        action: Option<AnyElement>,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .id(id)
            .test_support()
            .aria_label(SharedString::from(title.clone()))
            .w_full()
            .gap_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    // 1px between the two lines, as the other lists have it.
                    .gap(px(1.))
                    .child(
                        div()
                            .text_color(theme.foreground)
                            .strong(FontWeight::MEDIUM, cx)
                            .child(title),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(TEXT_SECONDARY)
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    ),
            )
            .children(action)
            .into_any_element()
    }

    fn render_runtimes(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let locked = self.runtimes_locked(cx);
        let rows: Vec<AnyElement> = (self.runtimes.iter().flatten())
            .map(|(component, version)| {
                let removing = self.removing.contains(component);
                let delete = {
                    let key = component.clone();
                    Button::new(runtime_delete_id(component))
                        .ghost()
                        .small()
                        .label(t!("prefs.runtime_delete").to_string())
                        .loading(removing)
                        .disabled(locked || removing)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_delete_runtime(&key, window, cx)
                        }))
                };
                Self::list_row(
                    runtime_row_id(component),
                    component.clone(),
                    version.clone(),
                    Some(delete.into_any_element()),
                    cx,
                )
                .into_any_element()
            })
            .collect();
        let empty = self.runtimes.as_ref().is_some_and(Vec::is_empty);
        Self::field(t!("prefs.runtimes").to_string(), cx)
            .child(v_flex().id("prefs-runtimes").children(rows))
            .when(empty, |f| {
                f.child(Self::hint(
                    "prefs-runtimes-empty",
                    t!("prefs.runtimes_empty").to_string(),
                    cx,
                ))
            })
            .when(locked && !empty, |f| {
                f.child(Self::hint(
                    "prefs-runtimes-busy",
                    t!("prefs.runtimes_busy").to_string(),
                    cx,
                ))
            })
    }

    fn render_detected(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let list = self.java_list.read(cx);
        let loading = list.is_loading();
        let installs = list.installs().map(<[_]>::to_vec);
        let rows: Vec<AnyElement> = (installs.iter().flatten())
            .map(|install| {
                let title = t!(
                    "prefs.java_row",
                    major = install.major,
                    version = install.version
                )
                .to_string();
                let id = ElementId::Name(format!("prefs-java-{}", install.path.display()).into());
                Self::list_row(id, title, install.path.display().to_string(), None, cx)
                    .into_any_element()
            })
            .collect();
        let status = if loading {
            Some(t!("prefs.detecting"))
        } else if installs.as_ref().is_some_and(Vec::is_empty) {
            Some(t!("prefs.detected_empty"))
        } else {
            None
        };
        Self::field(t!("prefs.detected").to_string(), cx)
            .child(v_flex().id("prefs-detected").children(rows))
            .children(status.map(|text| Self::hint("prefs-detected-status", text.to_string(), cx)))
            .child(
                h_flex().child(
                    Button::new("prefs-redetect")
                        .outline()
                        .small()
                        .icon(Icon::new(IconName::RefreshCw))
                        .label(t!("prefs.redetect").to_string())
                        .loading(loading)
                        .disabled(loading)
                        .on_click(cx.listener(|this, _, _, cx| {
                            redetect_java(&this.java_list, cx);
                        })),
                ),
            )
    }
}
