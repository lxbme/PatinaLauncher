use std::sync::Arc;

use gpui_kit::component::{
    list::{List, ListDelegate, ListItem, ListState},
    sidebar::{Sidebar, SidebarMenu, SidebarMenuItem},
    tab::{Tab, TabBar},
    IndexPath, Theme, ThemeMode,
};
use gpui_kit::*;

// crypto_pro DARK tokens.
const BG: u32 = 0x1a1b24;
const RAISED: u32 = 0x24262f;
const HEADER: u32 = 0x20222c;
const HOVER: u32 = 0x22252e;
const SELECTED: u32 = 0x1f2633;
const BORDER: u32 = 0x2c2e38;
const TEXT: u32 = 0xd1d4dc;
const MUTED: u32 = 0x8e939d;
const ACCENT: u32 = 0x2a99fb;
const TEAL: u32 = 0x1aaaa7;

fn c(v: u32) -> Hsla {
    Hsla::from(rgb(v))
}

fn theme_colors(t: &mut Theme) {
    t.background = c(BG);
    t.foreground = c(TEXT);
    t.muted = c(RAISED);
    t.muted_foreground = c(MUTED);
    t.border = c(BORDER);
    t.input = c(BORDER);
    t.ring = c(ACCENT);
    t.title_bar = c(HEADER);
    t.title_bar_border = c(BORDER);
    t.list_hover = c(HOVER);
    t.list_active = c(SELECTED);
    t.list_active_border = c(ACCENT);
    t.list_head = c(HEADER);
    t.primary = c(TEAL); // underline tab uses `primary`
    t.accent = c(RAISED);
    t.accent_foreground = c(TEXT);
    t.secondary = c(RAISED);
    t.secondary_hover = c(HOVER);
    t.secondary_active = c(SELECTED);
    t.secondary_foreground = c(TEXT);
    t.tab_bar = c(BG);
    t.tab = c(BG);
    t.tab_active = c(BG);
    t.tab_active_foreground = c(TEXT);
    t.tab_foreground = c(MUTED);
    t.sidebar = c(HEADER);
    t.sidebar_border = c(BORDER);
    t.sidebar_foreground = c(MUTED);
}

struct Instances;

impl ListDelegate for Instances {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        50
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _w: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        Some(
            ListItem::new(ix.row)
                .mx(px(6.))
                .px(px(8.))
                .rounded(px(4.))
                .text_size(rems(0.8125))
                .selected(ix.row == 0)
                .child(format!("实例 {}", ix.row + 1)),
        )
    }

    fn set_selected_index(
        &mut self,
        _: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
    }
}

struct Main {
    list: Entity<ListState<Instances>>,
}

impl Render for Main {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = ["概览", "模组", "资源包", "光影", "存档", "日志", "设置"];
        div()
            .size_full()
            .flex()
            .bg(c(BG))
            .text_color(c(TEXT))
            .text_size(rems(0.8125))
            .font_features(FontFeatures(Arc::new(vec![("tnum".into(), 1)])))
            .child(
                Sidebar::<SidebarMenu>::new("icons")
                    .w(px(46.))
                    .collapsed(true)
                    .child(
                        SidebarMenu::new()
                            .children(["A", "B", "C", "D"].map(SidebarMenuItem::new)),
                    ),
            )
            .child(
                div()
                    .w(px(150.))
                    .h_full()
                    .border_l_1()
                    .border_r_1()
                    .border_color(c(BORDER))
                    .py(px(8.))
                    .child(List::new(&self.list)),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(12.))
                            .pt(px(14.))
                            .pb(px(10.))
                            .text_size(rems(0.875))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Vanilla 1.21.1"),
                    )
                    .child(
                        TabBar::new("tabs")
                            .underline()
                            .selected_index(0)
                            .children(tabs.map(|t| Tab::new().label(t))),
                    ),
            )
    }
}

fn main() {
    gpui_kit::application().run(|cx: &mut App| {
        gpui_kit::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1100.), px(700.)),
                cx,
            ))),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            Theme::change(ThemeMode::Dark, Some(window), cx);
            Theme::update(cx, theme_colors);
            window.refresh();
            let list = cx.new(|cx| ListState::new(Instances, window, cx));
            cx.new(|_| Main { list })
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
