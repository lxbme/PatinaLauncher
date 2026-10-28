use gpui::{
    div, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds, Context, Window,
    WindowBounds, WindowOptions,
};

struct Root;

impl Render for Root {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = rgb(0x2c2e38);
        let tabs = ["概览", "模组", "资源包", "光影", "存档", "日志", "设置"];
        let mut root = div();
        root.text_style().get_or_insert_with(Default::default).font_features =
            Some(gpui::FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)])));
        root.size_full()
            .flex()
            .bg(rgb(0x1a1b24))
            .text_color(rgb(0xd1d4dc))
            .text_size(px(13.))
            .child(
                div()
                    .w(px(46.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .py(px(12.))
                    .bg(rgb(0x20222c))
                    .border_r_1()
                    .border_color(border)
                    .children(["A", "B", "C", "D"].map(|s| {
                        div()
                            .size(px(30.))
                            .rounded(px(4.))
                            .bg(rgb(0x24262f))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(s)
                    })),
            )
            .child(
                div()
                    .w(px(150.))
                    .h_full()
                    .border_r_1()
                    .border_color(border)
                    .bg(rgb(0x1a1b24))
                    .py(px(8.))
                    .child(
                        uniform_list(
                            "instances",
                            50,
                            cx.processor(|_t, range: std::ops::Range<usize>, _w, _cx| {
                                range
                                    .map(|ix| {
                                        div()
                                            .id(ix)
                                            .mx(px(6.))
                                            .px(px(8.))
                                            .py(px(5.))
                                            .rounded(px(4.))
                                            .when(ix == 0, |d| d.bg(rgb(0x1f2633)))
                                            .child(format!("实例 {}", ix + 1))
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .h_full(),
                    ),
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
                            .text_size(px(14.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Vanilla 1.21.1"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .px(px(12.))
                            .border_b_1()
                            .border_color(border)
                            .children(tabs.enumerate_tabs()),
                    ),
            )
    }
}

trait EnumTabs {
    fn enumerate_tabs(self) -> Vec<gpui::Div>;
}
impl EnumTabs for [&'static str; 7] {
    fn enumerate_tabs(self) -> Vec<gpui::Div> {
        self.into_iter()
            .enumerate()
            .map(|(i, t)| {
                let d = div().py_2().border_b_2().border_color(gpui::transparent_black());
                if i == 0 {
                    d.border_color(rgb(0x1aaaa7)).text_color(rgb(0xd1d4dc)).child(t)
                } else {
                    d.text_color(rgb(0x8e939d)).child(t)
                }
            })
            .collect()
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Root),
        )
        .unwrap();
        cx.activate(true);
    });
}
