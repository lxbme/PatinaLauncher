use super::*;
use crate::{accounts::open_login_dialog, layout::scroll_y};
use gpui_kit::component::{
    Disableable, Icon, IconName, Selectable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    radio::{Radio, RadioGroup},
    select::Select,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, Bounds, FocusHandle, FontWeight, InteractiveElement, IntoElement, ObjectFit,
    ParentElement, Pixels, Render, StatefulInteractiveElement, Styled, StyledImage, TestSupportExt,
    Window, div, img, point, px, rems,
};
use patina_core::accounts::appearance::{FieldOutcome, PreviewSide};
use std::cell::Cell;

struct CapeTileFocus {
    handle: FocusHandle,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    reveal_layout: Rc<Cell<Option<CapeTileLayout>>>,
    _focus_in: Subscription,
}

#[derive(Clone, Copy, PartialEq)]
struct CapeTileLayout {
    rem_size: Pixels,
    viewport_size: gpui_kit::Size<Pixels>,
}

fn reveal_cape_tile(tile: Bounds<Pixels>, scroll: &gpui_kit::ScrollHandle, window: &mut Window) {
    // Match the grid's p_1 inset so revealing keyboard focus preserves
    // room for Kit's outward ring, including after zoom.
    let inset = window.pixel_snap(window.rem_size() * 0.25);
    let viewport = scroll.bounds();
    let top = viewport.top() + inset;
    let bottom = viewport.bottom() - inset;
    let delta = if tile.top() < top || tile.size.height > bottom - top {
        top - tile.top()
    } else if tile.bottom() > bottom {
        bottom - tile.bottom()
    } else {
        Pixels::ZERO
    };
    if delta != Pixels::ZERO {
        let offset = scroll.offset();
        scroll.set_offset(point(offset.x, offset.y + delta));
        window.refresh();
    }
}

fn text(id: &'static str, value: String) -> impl IntoElement + Styled {
    div()
        .id(id)
        .test_support()
        .aria_label(value.clone())
        .child(value)
}
impl AppearancePage {
    fn status(&self) -> String {
        if self.model.is_applying() {
            return t!("appearance.applying").to_string();
        }
        if self.model.state() == AppearanceState::Reconciling {
            return t!("appearance.checking").to_string();
        }
        if self.model.state() == AppearanceState::Uncertain {
            return t!("appearance.uncertain").to_string();
        }
        if let Some(outcome) = self.model.outcome() {
            if outcome.skin() == &FieldOutcome::Confirmed
                && matches!(
                    outcome.cape(),
                    FieldOutcome::Failed(_) | FieldOutcome::Uncertain(_)
                )
            {
                return t!("appearance.partial_skin").to_string();
            }
            if outcome.refresh_error().is_some() {
                return t!("appearance.refresh_failed").to_string();
            }
        }
        if self.model.has_unapplied_changes() {
            t!("appearance.dirty").to_string()
        } else {
            String::new()
        }
    }
    fn render_preview(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let preview = if let Some(image) = &self.image {
            // Raster dimensions represent physical pixels, not product spacing.
            div()
                .id("appearance-raster")
                .test_support()
                .aria_label(t!("appearance.preview").to_string())
                .child(
                    img(image.clone())
                        .w(px(self.image_size.0 as f32 / window.scale_factor()))
                        .h(px(self.image_size.1 as f32 / window.scale_factor()))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element()
        } else {
            text(
                "appearance-preview-state",
                if self.model.state() == AppearanceState::Loading || self.loading_cape.is_some() {
                    t!("appearance.loading").to_string()
                } else {
                    t!("appearance.preview_unavailable").to_string()
                },
            )
            .into_any_element()
        };
        v_flex()
            .id("appearance-preview-column")
            .test_support()
            .w(rems(18.))
            .min_w(rems(14.))
            .flex_shrink(1.)
            .h_full()
            .gap_3()
            .child(
                div()
                    .id("appearance-preview")
                    .test_support()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .items_center()
                    .justify_center()
                    .bg(cx.theme().muted)
                    .rounded_lg()
                    .child(preview),
            )
            .child(
                h_flex()
                    .justify_center()
                    .gap_2()
                    .child(
                        Button::new("appearance-front")
                            .label(t!("appearance.front").to_string())
                            .selected(self.model.side() == PreviewSide::Front)
                            .disabled(self.model.is_applying())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.model.set_side(PreviewSide::Front);
                                this.rebuild_preview(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("appearance-back")
                            .label(t!("appearance.back").to_string())
                            .selected(self.model.side() == PreviewSide::Back)
                            .disabled(self.model.is_applying())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.model.set_side(PreviewSide::Back);
                                this.rebuild_preview(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex().justify_center().child(
                    Checkbox::new("appearance-outer")
                        .label(t!("appearance.outer").to_string())
                        .checked(self.model.outer())
                        .disabled(self.model.is_applying())
                        .on_change(cx.listener(|this, value, window, cx| {
                            this.model.set_outer(*value);
                            this.rebuild_preview(window, cx);
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }
    fn render_options(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.model.is_editable();
        let legacy = self.model.skin().is_some_and(SkinUpload::is_legacy);
        let model = self.model.skin().map(SkinUpload::model).or_else(|| {
            self.model
                .baseline()
                .and_then(|p| p.skin())
                .map(|s| s.model())
        });
        let mut grid = div()
            .id("appearance-cape-grid")
            .test_support()
            .flex()
            .flex_wrap()
            .content_start()
            .w_full()
            .flex_none()
            .gap_3()
            .p_1();
        let choices = std::iter::once((None, t!("appearance.no_cape").to_string())).chain(
            self.model
                .baseline()
                .into_iter()
                .flat_map(|p| p.capes())
                .map(|cape| (Some(cape.id().clone()), cape.alias().to_owned())),
        );
        for (id, name) in choices {
            let selected = self.model.selected_cape() == id.as_ref();
            let key = id.as_ref().map_or_else(
                || "appearance-cape-none".to_owned(),
                |id| format!("appearance-cape-{}", id.as_str()),
            );
            let thumbnail = div()
                .id(format!("{key}-thumbnail"))
                .test_support()
                .w(rems(2.5))
                .h(rems(4.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center();
            let thumbnail = if let Some(image) = id.as_ref().and_then(|id| self.thumbs.get(id)) {
                thumbnail.child(
                    img(image.clone())
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
            } else {
                thumbnail.child("—")
            };
            let scroll = self.scroll.clone();
            let scope = window.use_keyed_state(format!("{key}-focus"), cx, move |window, cx| {
                let handle = cx.focus_handle();
                let bounds = Rc::new(Cell::new(None::<Bounds<Pixels>>));
                let observed = bounds.clone();
                let focus_in = window.on_focus_in(&handle, cx, move |window, _| {
                    let Some(tile) = observed.get() else {
                        return;
                    };
                    reveal_cape_tile(tile, &scroll, window);
                });
                CapeTileFocus {
                    handle,
                    bounds,
                    reveal_layout: Rc::new(Cell::new(None)),
                    _focus_in: focus_in,
                }
            });
            let focus = scope.read(cx).handle.clone();
            let tile_bounds = scope.read(cx).bounds.clone();
            let reveal_layout = scope.read(cx).reveal_layout.clone();
            let observed_focus = focus.clone();
            let scroll = self.scroll.clone();
            grid = grid.child(
                div()
                    // The focus scope observes the real Button descendant, without
                    // replacing Kit's Tab/activation/focus-ring behavior.
                    .on_children_prepainted(move |children, window, cx| {
                        let tile = children.first().copied();
                        tile_bounds.set(tile);
                        let layout = CapeTileLayout {
                            rem_size: window.rem_size(),
                            viewport_size: scroll.bounds().size,
                        };
                        if reveal_layout.replace(Some(layout)) != Some(layout)
                            && observed_focus.contains_focused(window, cx)
                            && let Some(tile) = tile
                        {
                            // Reflow invalidates pixel scroll offsets. Reveal only
                            // on layout changes, not after intentional wheel scrolling.
                            reveal_cape_tile(tile, &scroll, window);
                        }
                    })
                    .id(format!("{key}-scope"))
                    .w(rems(7.5))
                    .h(rems(10.))
                    .flex_none()
                    .track_focus(&focus)
                    .child(
                        Button::new(key.clone())
                            .size_full()
                            .p_0()
                            .border_2()
                            .border_color(if selected {
                                cx.theme().primary
                            } else {
                                cx.theme().border
                            })
                            .selected(selected)
                            .disabled(!editable)
                            .accessibility_label(name.clone())
                            .child(
                                v_flex()
                                    .relative()
                                    .size_full()
                                    .items_center()
                                    .gap_2()
                                    .px_2()
                                    .py_3()
                                    .child(thumbnail)
                                    .child(
                                        div()
                                            .id(format!("{key}-name"))
                                            .test_support()
                                            .aria_label(name.clone())
                                            .w_full()
                                            .h(rems(2.5))
                                            .text_center()
                                            .whitespace_normal()
                                            .line_clamp(2)
                                            .child(name),
                                    )
                                    .when(selected, |d| {
                                        d.child(
                                            div()
                                                .id(format!("{key}-check"))
                                                .test_support()
                                                .absolute()
                                                .top_1()
                                                .right_1()
                                                .text_color(cx.theme().primary)
                                                .child(Icon::new(IconName::Check).size_4()),
                                        )
                                    }),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.model.set_cape(id.clone()) {
                                    this.load_cape(window, cx);
                                    cx.notify();
                                }
                            })),
                    ),
            );
        }
        let capes = v_flex().id("appearance-capes").test_support().child(grid);
        v_flex()
            .id("appearance-options")
            .test_support()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .gap_6()
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("appearance.skin").to_string()),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .min_w_0()
                            .child(
                                Button::new("appearance-choose")
                                    .label(t!("appearance.choose").to_string())
                                    .disabled(!editable)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_skin(window, cx)
                                    })),
                            )
                            .child(
                                text(
                                    "appearance-filename",
                                    self.model.filename().map(str::to_owned).unwrap_or_else(|| {
                                        t!("appearance.current_skin").to_string()
                                    }),
                                )
                                .flex_1()
                                .min_w_0()
                                .text_ellipsis(),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(t!("appearance.model").to_string())
                            .child(
                                RadioGroup::horizontal("appearance-model")
                                    .selected_index(
                                        model.map(|model| usize::from(model == SkinModel::Slim)),
                                    )
                                    .disabled(!editable || self.model.skin().is_none())
                                    .child(
                                        Radio::new("appearance-classic")
                                            .label(t!("appearance.classic").to_string()),
                                    )
                                    .child(
                                        Radio::new("appearance-slim")
                                            .label(t!("appearance.slim").to_string())
                                            .disabled(legacy),
                                    )
                                    .on_change(cx.listener(|this, ix, window, cx| {
                                        if let Err(error) = this.model.set_model(if *ix == 0 {
                                            SkinModel::Classic
                                        } else {
                                            SkinModel::Slim
                                        }) {
                                            this.model.set_error(error);
                                        }
                                        this.rebuild_preview(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(legacy, |d| {
                        d.child(text(
                            "appearance-legacy",
                            t!("appearance.legacy").to_string(),
                        ))
                    })
                    .when_some(self.skin_error.as_ref(), |d, error| {
                        d.child(text("appearance-texture-error", error.clone()))
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("appearance.cape").to_string()),
                    )
                    .when_some(self.cape_error.as_ref(), |d, error| {
                        d.child(text("appearance-cape-error", error.clone()))
                    })
                    // The viewport owns only the space left below the cape heading.
                    // A full-height wrapper with an automatic minimum overflows its section.
                    .child(
                        scroll_y(capes, &self.scroll)
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden(),
                    ),
            )
            .into_any_element()
    }
}
impl Render for AppearancePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(any(test, feature = "test-support"))]
        {
            self.render_count += 1;
        }
        let has_accounts = !self.model.accounts().is_empty();
        let mut page = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .p_6()
            .gap_6()
            .text_sm()
            .child(
                h_flex()
                    .id("appearance-header")
                    .test_support()
                    .w_full()
                    .flex_none()
                    .gap_6()
                    .child(
                        div()
                            .flex_1()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("appearance.title").to_string()),
                    )
                    .when(has_accounts, |d| {
                        d.child(
                            h_flex()
                                .gap_3()
                                .child(t!("appearance.account").to_string())
                                .child(
                                    Select::new(&self.selector)
                                        .id("appearance-account")
                                        .accessibility_label(t!("appearance.account").to_string())
                                        .w(rems(12.))
                                        .disabled(
                                            self.model.is_applying()
                                                || self.model.state()
                                                    == AppearanceState::Reconciling,
                                        ),
                                ),
                        )
                    }),
            );
        if !has_accounts {
            page = page.child(
                v_flex()
                    .id("appearance-body")
                    .test_support()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .when_some(self.model.error(), |d, error| {
                        d.child(text("appearance-error", error.to_owned()))
                    })
                    .child(
                        Button::new("appearance-login")
                            .label(t!("appearance.login").to_string())
                            .on_click(|_, window, cx| {
                                open_login_dialog(window, cx);
                            }),
                    ),
            );
        } else if self.model.baseline().is_none() {
            page = page.child(
                v_flex()
                    .id("appearance-body")
                    .test_support()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .child(text(
                        "appearance-error",
                        self.model
                            .error()
                            .map(str::to_owned)
                            .unwrap_or_else(|| t!("appearance.loading").to_string()),
                    ))
                    .when(self.reauth_required, |d| {
                        d.child(
                            Button::new("appearance-sign-in")
                                .label(t!("appearance.sign_in").to_string())
                                .on_click(|_, window, cx| {
                                    open_login_dialog(window, cx);
                                }),
                        )
                    })
                    .when(self.model.state() == AppearanceState::Unavailable, |d| {
                        d.child(
                            Button::new("appearance-retry")
                                .label(t!("appearance.retry").to_string())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if this.model.reload() {
                                        this.clear_assets(cx);
                                        this.load_profile(window, cx);
                                        cx.notify();
                                    }
                                })),
                        )
                    }),
            );
        } else {
            page = page.child(
                h_flex()
                    .id("appearance-body")
                    .test_support()
                    .items_stretch()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .gap_8()
                    .child(self.render_preview(window, cx))
                    .child(self.render_options(window, cx)),
            );
        }
        let status = self.status();
        page.child(
            v_flex()
                .id("appearance-footer")
                .test_support()
                .w_full()
                .flex_none()
                .gap_2()
                .border_t_1()
                .border_color(cx.theme().border)
                .pt_4()
                .when_some(
                    self.model
                        .error()
                        .filter(|_| self.model.baseline().is_some()),
                    |d, error| {
                        d.child(
                            text("appearance-error-detail", error.to_owned())
                                .text_color(cx.theme().danger),
                        )
                    },
                )
                .child(
                    h_flex()
                        .gap_3()
                        .when(
                            self.reauth_required && self.model.baseline().is_some(),
                            |d| {
                                d.child(
                                    Button::new("appearance-sign-in-apply")
                                        .label(t!("appearance.sign_in").to_string())
                                        .on_click(|_, window, cx| {
                                            open_login_dialog(window, cx);
                                        }),
                                )
                            },
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(text("appearance-status", status)),
                        )
                        .when(
                            matches!(
                                self.model.state(),
                                AppearanceState::Uncertain | AppearanceState::Unavailable
                            ) && self.model.outcome().is_some(),
                            |d| {
                                d.child(
                                    Button::new("appearance-reconcile")
                                        .label(t!("appearance.reconcile").to_string())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reconcile(window, cx)
                                        })),
                                )
                            },
                        )
                        .child(
                            Button::new("appearance-discard")
                                .label(t!("appearance.discard").to_string())
                                .disabled(
                                    !self.model.has_unapplied_changes() || self.model.is_applying(),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.model.discard();
                                    this.load_cape(window, cx);
                                    this.rebuild_preview(window, cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("appearance-apply")
                                .primary()
                                .label(t!("appearance.apply").to_string())
                                .disabled(
                                    !self.model.has_unapplied_changes()
                                        || !self.model.is_editable(),
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.apply(window, cx)),
                                ),
                        ),
                ),
        )
    }
}
