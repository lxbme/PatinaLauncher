//! Detail pane of the selected instance: header block (icon, name, meta
//! line, 启动 and ⋯), underline tabs, then the tab's content.

mod log;
mod overview;

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    Anchor, AnyElement, Context, FontWeight, ElementId, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, rems,
};

use crate::data::{self, Instance, Level, LogLine};
use crate::state::AppState;
use crate::theme::{Palette, shade};
use crate::ui::{
    GUTTER, Strong, TEXT_SECONDARY, empty_state, icon_tile, loader_badge, secondary_button,
    underline_tab,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Mods,
    ResourcePacks,
    Shaders,
    Worlds,
    Settings,
    Log,
}

impl Tab {
    const ALL: [Tab; 7] = [
        Tab::Overview,
        Tab::Mods,
        Tab::ResourcePacks,
        Tab::Shaders,
        Tab::Worlds,
        Tab::Settings,
        Tab::Log,
    ];

    fn label(self) -> &'static str {
        match self {
            Tab::Overview => "概览",
            Tab::Mods => "Mods",
            Tab::ResourcePacks => "资源包",
            Tab::Shaders => "光影",
            Tab::Worlds => "存档",
            Tab::Settings => "设置",
            Tab::Log => "日志",
        }
    }
}

pub struct DetailView {
    state: Entity<AppState>,
    tab: Tab,
    /// Log level filter chip; `None` shows every line.
    log_filter: Option<Level>,
    log: Vec<LogLine>,
    _observe: Subscription,
}

impl DetailView {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&state, |_, _, cx| cx.notify());
        let tab = match std::env::var("PATINA_TAB").as_deref() {
            Ok("log") => Tab::Log,
            Ok("mods") => Tab::Mods,
            _ => Tab::Overview,
        };
        Self {
            state,
            tab,
            log_filter: None,
            log: data::launch_log(),
            _observe,
        }
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            cx.notify();
        }
    }

    fn render_header(&self, inst: &Instance, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::get(cx);
        let sep = || div().text_color(p.border).child("|");
        let launch = ButtonCustomVariant::new(cx)
            .color(p.teal)
            .foreground(gpui_kit::white())
            .hover(shade(p.teal, 0.05))
            .active(shade(p.teal, -0.05));
        let more = ButtonCustomVariant::new(cx)
            .color(p.raised)
            .foreground(p.text)
            .hover(shade(p.raised, 0.04))
            .active(p.selected);
        h_flex()
            .flex_none()
            .px(GUTTER)
            .pt(rems(1.125))
            .pb(rems(1.))
            .gap(rems(0.875))
            .child(icon_tile(
                inst.initial,
                p.tint(inst.tint),
                rems(3.5),
                rems(1.375),
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(rems(0.375))
                    .child(
                        div()
                            .text_size(rems(1.375))
                            .line_height(rems(1.75))
                            .strong(FontWeight::SEMIBOLD)
                            .text_color(p.text)
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(inst.name),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .text_size(TEXT_SECONDARY)
                            .text_color(p.muted)
                            .whitespace_nowrap()
                            .child(loader_badge(inst.loader_full(), inst.loader, cx))
                            .child(inst.mc)
                            .child(sep())
                            .child(format!("Java {}", inst.java))
                            .child(sep())
                            .child(format!("上次游玩 {}", inst.last_played)),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(
                        Button::new("launch")
                            .custom(launch)
                            // A custom variant's resting fill is drawn at 20 %
                            // opacity by gpui-component; set it solid.
                            .bg(p.teal)
                            .icon(IconName::Play)
                            .label("启动")
                            .h(rems(2.25))
                            .px(rems(1.25))
                            .gap_1p5()
                            .text_size(rems(0.875))
                            .strong(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .map(|b| Styled::rounded(b, rems(0.375))),
                    )
                    .child(
                        Button::new("instance-more")
                            .custom(more)
                            .bg(p.raised)
                            .icon(IconName::Ellipsis)
                            .size(rems(2.25))
                            .cursor_pointer()
                            .map(|b| Styled::rounded(b, rems(0.375)))
                            .dropdown_menu_with_anchor(Anchor::TopRight, |menu, _, _| {
                                menu.item(PopupMenuItem::new("打开实例文件夹").icon(IconName::FolderOpen))
                                    .item(PopupMenuItem::new("复制实例").icon(IconName::Copy))
                                    .item(
                                        PopupMenuItem::new("导出整合包")
                                            .icon(gpui_kit::assets::IconName::Package),
                                    )
                                    .separator()
                                    .item(
                                        PopupMenuItem::new("删除实例")
                                            .icon(gpui_kit::assets::IconName::Trash),
                                    )
                            }),
                    ),
            )
    }

    fn render_tabs(&self, inst: &Instance, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::get(cx);
        let this = cx.weak_entity();
        h_flex()
            .flex_none()
            .h(rems(2.25))
            .px(GUTTER)
            .gap_5()
            .border_b_1()
            .border_color(p.border)
            .children(Tab::ALL.into_iter().map(|t| {
                let this = this.clone();
                let count = match t {
                    Tab::Mods if inst.mods > 0 => Some(inst.mods),
                    Tab::Worlds => Some(inst.saves),
                    _ => None,
                };
                underline_tab(
                    ElementId::Name(format!("tab-{t:?}").into()),
                    t.label(),
                    count,
                    t == self.tab,
                    move |_, _, cx| {
                        let _ = this.update(cx, |v, cx| v.set_tab(t, cx));
                    },
                    cx,
                )
            }))
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        use gpui_kit::assets::IconName as Lucide;
        let (icon, text, action): (gpui_kit::component::Icon, &str, Option<&str>) = match self.tab
        {
            Tab::Mods => (Lucide::Puzzle.into(), "从 Modrinth 或 CurseForge 浏览并安装 Mod", Some("浏览 Mod")),
            Tab::ResourcePacks => (Lucide::Image.into(), "这个实例还没有资源包", Some("添加资源包")),
            Tab::Shaders => (Lucide::SunMedium.into(), "未安装光影包，需要 Iris 或 OptiFine", Some("浏览光影")),
            Tab::Worlds => (Lucide::Earth.into(), "进入游戏后创建的世界会出现在这里", None),
            _ => (IconName::Settings2.into(), "内存、Java 与启动参数的实例级设置", Some("编辑设置")),
        };
        let button = action.map(|a| secondary_button("empty-action", a));
        empty_state(icon, text, button, cx).into_any_element()
    }
}

impl Render for DetailView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::get(cx);
        let inst = self.state.read(cx).selected().clone();
        let body = match self.tab {
            Tab::Overview => overview::render(&inst, cx).into_any_element(),
            Tab::Log => self.render_log(cx).into_any_element(),
            _ => self.render_empty(cx),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(p.bg)
            .child(self.render_header(&inst, cx))
            .child(self.render_tabs(&inst, cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}
