//! The add-instance dialog: name and icon on the left, the Minecraft
//! version and the loader on the right, "取消" and "创建" below.
//!
//! [`AddInstanceForm`] holds what the user has chosen so far; the version
//! lists belong to the [`VersionPicker`] and [`LoaderPicker`] it embeds.
//! The name follows the choices (`Fabric 1.21.1`, `1.21.1`) until the user
//! edits it. "创建" (also Enter) is disabled while the name is empty, no
//! version is chosen or the loader's versions are still loading.
//!
//! Creating closes the dialog at once and continues in the background
//! ([`create_instance`]): the instance is created and given its icon, the
//! list is refreshed and the new instance selected, then the loader (if any)
//! installs as an operation on the instance's header, where a failure shows
//! with "重试" and the instance stays vanilla.

use std::cell::Cell;
use std::rc::Rc;

use futures::FutureExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme, Disableable, WindowExt, h_flex, v_flex};
use gpui_kit::{
    App, AppContext, Context, Entity, EventEmitter, Focusable, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, TestSupportExt, Window, div, rems,
};
use patina_core::loader::ops::OpOutcome;
use patina_core::loader::{LoaderKind, VersionPick};
use rust_i18n::t;

use super::icon::{IconChoice, save_icon};
use super::icon_picker::{IconPicked, IconPicker};
use super::model::{InstancesChanged, InstancesModel};
use super::version_picker::{LoaderChanged, LoaderPicker, VersionChosen, VersionPicker};
use crate::core_bridge::{Core, run_blocking};
use crate::fonts::Strong;
use crate::launch::{OpError, OpKind, RetryAction};
use crate::layout::{TEXT_PRIMARY, TEXT_SECONDARY};

/// Dialog width at UI scale 1.0 (800 px): the icon grid and the version
/// lists side by side.
const DIALOG_W: f32 = 50.;

/// The name an instance gets from its choices: `"<loader> <mc>"`, or the
/// Minecraft version alone without a loader.
pub fn default_name(loader: Option<LoaderKind>, mc: &str) -> String {
    match loader {
        Some(kind) => format!("{} {mc}", kind.display_name()),
        None => mc.to_string(),
    }
}

/// What "创建" creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInstance {
    pub name: String,
    pub mc: String,
    pub icon: IconChoice,
    /// The loader and its exact version.
    pub loader: Option<(LoaderKind, String)>,
}

/// The dialog's form (an entity): the name input, the icon, and the two
/// pickers. See the module docs.
pub struct AddInstanceForm {
    model: Entity<InstancesModel>,
    name: Entity<InputState>,
    /// Set once the user has typed in the name; it then stops following
    /// the choices.
    name_edited: bool,
    icon: IconChoice,
    icon_picker: Entity<IconPicker>,
    versions: Entity<VersionPicker>,
    loaders: Entity<LoaderPicker>,
    /// The Minecraft version chosen in `versions`.
    mc: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// Emitted when whether the form can be submitted may have changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormChanged;

impl EventEmitter<FormChanged> for AddInstanceForm {}

impl AddInstanceForm {
    pub fn new(model: Entity<InstancesModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx));
        let icon_picker = cx.new(|cx| IconPicker::new(IconChoice::None, cx));
        let versions = cx.new(|cx| VersionPicker::new(None, window, cx));
        let loaders = cx.new(LoaderPicker::new);
        let _subscriptions = vec![
            cx.subscribe(&name, |this, _, event: &InputEvent, cx| {
                // `set_value` (the generated name) emits no Change: only
                // the user's typing does.
                if matches!(event, InputEvent::Change) {
                    this.name_edited = true;
                    this.changed(cx);
                }
            }),
            cx.subscribe(
                &icon_picker,
                |this, _, IconPicked(choice): &IconPicked, _| {
                    this.icon = choice.clone();
                },
            ),
            cx.subscribe_in(
                &versions,
                window,
                |this, _, VersionChosen(mc): &VersionChosen, window, cx| {
                    this.mc = Some(mc.clone());
                    this.loaders
                        .update(cx, |l, cx| l.set_mc(Some(mc.clone()), cx));
                    this.follow_choices(window, cx);
                },
            ),
            cx.subscribe_in(
                &loaders,
                window,
                |this, _, _: &LoaderChanged, window, cx| {
                    this.follow_choices(window, cx);
                },
            ),
        ];
        Self {
            model,
            name,
            name_edited: false,
            icon: IconChoice::None,
            icon_picker,
            versions,
            loaders,
            mc: None,
            _subscriptions,
        }
    }

    pub fn name_input(&self) -> &Entity<InputState> {
        &self.name
    }

    pub fn versions(&self) -> &Entity<VersionPicker> {
        &self.versions
    }

    pub fn loaders(&self) -> &Entity<LoaderPicker> {
        &self.loaders
    }

    /// What "创建" would create now, or `None` while it is disabled.
    pub fn request(&self, cx: &App) -> Option<NewInstance> {
        let name = self.name.read(cx).value().trim().to_string();
        let loaders = self.loaders.read(cx);
        if name.is_empty() || !loaders.is_ready() {
            return None;
        }
        let loader = match (loaders.kind(), loaders.version()) {
            (Some(kind), Some(version)) => Some((kind, version.to_string())),
            _ => None,
        };
        Some(NewInstance {
            name,
            mc: self.mc.clone()?,
            icon: self.icon.clone(),
            loader,
        })
    }

    /// Whether Enter commits the dialog with the keyboard where it is: in
    /// the name, the version list or the loader version list.
    pub fn commits_on_enter(&self, window: &Window, cx: &App) -> bool {
        self.name.focus_handle(cx).is_focused(window)
            || self.versions.read(cx).list_focus().is_focused(window)
            || self.loaders.read(cx).list_focus().is_focused(window)
    }

    /// Starts creating the instance if the form is complete; returns whether
    /// it did (the dialog then closes).
    pub fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(request) = self.request(cx) else {
            return false;
        };
        create_instance(self.model.clone(), request, window, cx);
        true
    }

    /// Puts the generated name in the name input unless the user edited it.
    fn follow_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.name_edited
            && let Some(mc) = &self.mc
        {
            let name = default_name(self.loaders.read(cx).kind(), mc);
            if self.name.read(cx).value() != name.as_str() {
                self.name
                    .update(cx, |input, cx| input.set_value(name, window, cx));
            }
        }
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(FormChanged);
        cx.notify();
    }

    fn label(text: String, cx: &App) -> impl IntoElement {
        div()
            .text_size(TEXT_SECONDARY)
            .strong(FontWeight::MEDIUM, cx)
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }
}

impl Render for AddInstanceForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("add-instance-form")
            .test_support()
            .w_full()
            .items_start()
            .gap_6()
            .text_size(TEXT_PRIMARY)
            .child(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .child(Self::label(t!("add_instance.name").to_string(), cx))
                    .child(Input::new(&self.name).id("add-instance-name"))
                    .child(div().h_2())
                    .child(Self::label(t!("add_instance.icon").to_string(), cx))
                    .child(self.icon_picker.clone()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(Self::label(t!("add_instance.game_version").to_string(), cx))
                    .child(self.versions.clone())
                    .child(div().h_2())
                    .child(Self::label(t!("add_instance.loader").to_string(), cx))
                    .child(self.loaders.clone()),
            )
    }
}

/// "取消" and "创建" (an entity, so "创建" follows the form).
struct AddInstanceFooter {
    form: Entity<AddInstanceForm>,
    _changed: Subscription,
}

impl AddInstanceFooter {
    fn new(form: Entity<AddInstanceForm>, cx: &mut Context<Self>) -> Self {
        let _changed = cx.subscribe(&form, |_, _, _: &FormChanged, cx| cx.notify());
        Self { form, _changed }
    }
}

impl Render for AddInstanceFooter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self.form.read(cx).request(cx).is_some();
        let form = self.form.clone();
        DialogFooter::new()
            .child(
                Button::new("add-instance-cancel")
                    .label(t!("add_instance.cancel").to_string())
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("add-instance-create")
                    .primary()
                    .label(t!("add_instance.create").to_string())
                    .disabled(!ready)
                    .on_click(move |_, window, cx| {
                        if form.update(cx, |f, cx| f.create(window, cx)) {
                            window.close_dialog(cx);
                        }
                    }),
            )
    }
}

/// Opens the add-instance dialog over `window`; the new instance goes into
/// `model`.
pub fn open(model: Entity<InstancesModel>, window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| AddInstanceForm::new(model, window, cx));
    let footer = cx.new(|cx| AddInstanceFooter::new(form.clone(), cx));
    // The builder runs again whenever the dialog layer is rebuilt; the name
    // takes the keyboard on the first run only.
    let first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            let name = form.read(cx).name_input().clone();
            name.update(cx, |input, cx| input.focus(window, cx));
        }
        let title = SharedString::from(t!("add_instance.title"));
        let form_ok = form.clone();
        dialog
            .w(rems(DIALOG_W).to_pixels(window.rem_size()))
            .title(
                div()
                    .id("add-instance-title")
                    .aria_label(title.clone())
                    .child(title)
                    .test_support(),
            )
            .child(form.clone())
            .footer(footer.clone())
            // Enter commits ("创建") only from the name and the two lists;
            // on any other control it is that control's own key (a button
            // presses itself), so the action goes on to its key listeners.
            .on_ok(move |_, window, cx| {
                if !form_ok.read(cx).commits_on_enter(window, cx) {
                    cx.propagate();
                    return false;
                }
                form_ok.update(cx, |f, cx| f.create(window, cx))
            })
    });
}

/// Creates `request` in the background (see the module docs). Failures are
/// reported as notifications on `window`.
pub fn create_instance(
    model: Entity<InstancesModel>,
    request: NewInstance,
    window: &mut Window,
    cx: &mut App,
) {
    let store = Core::global(cx).store.clone();
    let NewInstance {
        name,
        mc,
        icon,
        loader,
    } = request;
    let job = {
        let name = name.clone();
        run_blocking(cx, move || store.create(&name, &mc))
    };
    let weak = model.downgrade();
    window
        .spawn(cx, async move |cx| {
            let notify = |text: String, cx: &mut gpui_kit::AsyncWindowContext| {
                cx.update(|window, cx| window.push_notification(Notification::error(text), cx))
                    .ok();
            };
            let error = match job.await {
                Ok(Ok(instance)) => Ok(instance),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let instance = match error {
                Ok(instance) => instance,
                Err(error) => {
                    tracing::warn!(%error, name, "creating an instance failed");
                    notify(
                        t!("add_instance.create_failed", name = name, error = error).to_string(),
                        cx,
                    );
                    return;
                }
            };
            let id = instance.id;
            if icon != IconChoice::None {
                let Ok(saved) = cx.update(|_, cx| save_icon(&model, id.clone(), icon, cx)) else {
                    return;
                };
                if let Err(error) = saved.await {
                    notify(
                        t!("add_instance.icon_failed", name = name, error = error).to_string(),
                        cx,
                    );
                }
            }
            // `start_op` judges "still listed" against the loaded list, so
            // the new instance must be in it first.
            let Ok(refreshed) = weak.update(cx, |m, cx| m.refresh_and_wait(cx)) else {
                return;
            };
            refreshed.await;
            let unlisted = weak.update(cx, |m, cx| {
                if m.entry(&id).is_none() {
                    // Listing failed: the model keeps the old list and why.
                    let error = m.error().cloned().unwrap_or_default();
                    return Some(error);
                }
                m.select(&id, cx);
                cx.emit(InstancesChanged);
                if let Some((kind, version)) = loader.clone() {
                    install(m, &id, kind, version, cx);
                }
                None
            });
            if let Ok(Some(error)) = unlisted {
                tracing::warn!(%error, id, "the new instance is not in the refreshed list");
                let key = if loader.is_some() {
                    "add_instance.refresh_failed_loader"
                } else {
                    "add_instance.refresh_failed"
                };
                notify(t!(key, name = name, error = error).to_string(), cx);
            }
        })
        .detach();
}

/// Installs `version` of loader `kind` into instance `id` as an operation
/// of the model; its retry action installs again.
fn install(
    model: &mut InstancesModel,
    id: &str,
    kind: LoaderKind,
    version: String,
    cx: &mut Context<InstancesModel>,
) {
    let ops = Core::global(cx).version_ops.clone();
    let retry: RetryAction = {
        let (id, version) = (id.to_string(), version.clone());
        Rc::new(move |model, cx| install(model, &id, kind, version.clone(), cx))
    };
    let target = id.to_string();
    model.start_op(
        id,
        OpKind::InstallLoader,
        move |progress| {
            async move {
                let pick = VersionPick::Exact(version);
                (ops.install_loader(target, kind, pick, false, progress))
                    .await
                    .map_err(OpError::from)
            }
            .boxed()
        },
        |_, _: OpOutcome, _| {},
        Some(retry),
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_joins_loader_and_version() {
        let _locale = crate::i18n::locale_lock();
        crate::i18n::set_language(crate::i18n::Language::En);
        assert_eq!(default_name(None, "1.21.1"), "1.21.1");
        assert_eq!(
            default_name(Some(LoaderKind::Fabric), "1.21.1"),
            "Fabric 1.21.1"
        );
        assert_eq!(
            default_name(Some(LoaderKind::Neoforge), "1.20.1"),
            "NeoForge 1.20.1"
        );
    }
}
