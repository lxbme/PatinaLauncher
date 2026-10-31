//! The global search's results popup: the three groups (instances,
//! installed mods, online) with their headers, the chosen row and the
//! rows' identities.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Icon, ThemeStyled as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement, IntoElement, MouseDownEvent, ParentElement,
    Rems, StatefulInteractiveElement as _, Styled, TestSupportExt as _, div, rems,
};
use rust_i18n::t;

use super::GlobalSearch;
use super::model::{InstanceResult, ModGroup, ModResult, SearchItem, SearchResults};
use crate::content::{ImageKey, icon_tile, read_icon_file};
use crate::instances::instance_icon;
use crate::layout::{TEXT_PRIMARY, TEXT_SECONDARY};

/// Height of a result row (28 px).
const ROW_H: Rems = Rems(1.75);
/// Icon size in a result row (18 px).
const ICON: Rems = Rems(1.125);
/// The popup's width; wider than the box so a mod row has room for its
/// instance's name.
const WIDTH: Rems = Rems(26.);
/// Tallest the popup gets before its list scrolls.
const MAX_H: Rems = Rems(24.);
/// How a disabled mod's icon fades, as the Mods tab draws it.
const DISABLED_ICON_OPACITY: f32 = 0.5;
/// The image cache's instance slot for icons of the mod index: no instance
/// id contains `#`, and the icon files are named by their content's hash.
const INDEX_ICONS: &str = "#mod-index";

/// Element id of result `item`'s row.
pub fn search_row_id(item: &SearchItem) -> ElementId {
    match item {
        SearchItem::Instance(id) => ElementId::Name(format!("search-instance-{id}").into()),
        SearchItem::Mod {
            instance_id,
            file_name,
        } => ElementId::Name(format!("search-mod-{instance_id}/{file_name}").into()),
        SearchItem::Online => ElementId::Name("search-online".into()),
    }
}

/// A group of results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Instances,
    Mods,
    Online,
}

impl Group {
    fn id(self) -> &'static str {
        match self {
            Group::Instances => "search-group-instances",
            Group::Mods => "search-group-mods",
            Group::Online => "search-group-online",
        }
    }

    fn label(self) -> String {
        match self {
            Group::Instances => t!("shell.search.group.instances"),
            Group::Mods => t!("shell.search.group.mods"),
            Group::Online => t!("shell.search.group.online"),
        }
        .to_string()
    }
}

/// One line of the popup's list, in order.
enum Line<'a> {
    Header(Group),
    Instance(&'a InstanceResult),
    Mod(&'a ModResult),
    Online,
    /// The mod group while the index is still being built.
    Indexing,
    /// The mod group when building the index failed, with why.
    IndexFailed(&'a str),
    /// Neither an instance nor a mod matches.
    NoLocal,
}

/// The popup's lines: a group shows when it has results (the mods group
/// also while the index is being built); online is always there.
fn lines(results: &SearchResults) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    if !results.instances.is_empty() {
        lines.push(Line::Header(Group::Instances));
        lines.extend(results.instances.iter().map(Line::Instance));
    }
    match &results.mods {
        ModGroup::Indexing => lines.extend([Line::Header(Group::Mods), Line::Indexing]),
        ModGroup::Failed(error) => {
            lines.extend([Line::Header(Group::Mods), Line::IndexFailed(error)]);
        }
        ModGroup::Hits(mods) if !mods.is_empty() => {
            lines.push(Line::Header(Group::Mods));
            lines.extend(mods.iter().map(Line::Mod));
        }
        ModGroup::Hits(_) if results.instances.is_empty() => lines.push(Line::NoLocal),
        ModGroup::Hits(_) => {}
    }
    lines.extend([Line::Header(Group::Online), Line::Online]);
    lines
}

/// The index among the list's children of the row of result `cursor`, to
/// scroll it into view.
pub(super) fn child_index(results: &SearchResults, cursor: usize) -> usize {
    lines(results)
        .iter()
        .enumerate()
        .filter(|(_, line)| matches!(line, Line::Instance(_) | Line::Mod(_) | Line::Online))
        .nth(cursor)
        .map_or(0, |(ix, _)| ix)
}

impl GlobalSearch {
    pub(super) fn render_popup(
        &self,
        results: &SearchResults,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mut item_ix = 0;
        let mut children: Vec<AnyElement> = Vec::new();
        for line in lines(results) {
            let child = match line {
                Line::Header(group) => self.render_header(group, cx),
                Line::Indexing => {
                    self.render_note("search-indexing", t!("shell.search.indexing"), cx)
                }
                Line::IndexFailed(error) => self.render_note(
                    "search-index-failed",
                    t!("shell.search.index_failed", error = error),
                    cx,
                ),
                Line::NoLocal => {
                    self.render_note("search-no-local", t!("shell.search.no_local"), cx)
                }
                Line::Instance(hit) => {
                    let content = self.render_instance(hit, cx);
                    let item = SearchItem::Instance(hit.id.clone());
                    self.render_row(item, hit.name.clone(), item_ix, content, cx)
                }
                Line::Mod(hit) => {
                    let label = mod_label(hit);
                    let content = self.render_mod(hit, cx);
                    let item = SearchItem::Mod {
                        instance_id: hit.hit.instance_id.clone(),
                        file_name: hit.hit.file_name.clone(),
                    };
                    self.render_row(item, label, item_ix, content, cx)
                }
                Line::Online => {
                    let text = t!("shell.search.online_soon").to_string();
                    let content = self.render_online(text.clone(), cx);
                    self.render_row(SearchItem::Online, text, item_ix, content, cx)
                }
            };
            if matches!(line, Line::Instance(_) | Line::Mod(_) | Line::Online) {
                item_ix += 1;
            }
            children.push(child);
        }
        v_flex()
            .id("search-popup")
            .w(WIDTH)
            .popover_style(cx)
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                this.clicked_outside(event.position, cx)
            }))
            .test_support()
            .child(
                div()
                    .relative()
                    .child(
                        v_flex()
                            .id("search-results")
                            .max_h(MAX_H)
                            .p_1()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .children(children),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
    }

    fn render_header(&self, group: Group, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        div()
            .id(group.id())
            .px_2()
            .pt_1p5()
            .pb_0p5()
            .text_size(TEXT_SECONDARY)
            .text_color(theme.muted_foreground)
            .child(group.label())
            .test_support()
            .into_any_element()
    }

    fn render_note(
        &self,
        id: &'static str,
        text: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        div()
            .id(id)
            .px_2()
            .h(ROW_H)
            .flex()
            .items_center()
            .text_size(TEXT_PRIMARY)
            .text_color(theme.muted_foreground)
            .child(text.into())
            .test_support()
            .into_any_element()
    }

    /// A result's row: chosen with the keyboard (marked), opened with a
    /// click.
    fn render_row(
        &self,
        item: SearchItem,
        label: String,
        ix: usize,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let chosen = ix == self.cursor;
        let (hover, active) = (theme.list_hover, theme.list_active);
        h_flex()
            .id(search_row_id(&item))
            .w_full()
            .h(ROW_H)
            .flex_none()
            .px_2()
            .gap_2()
            .rounded(theme.radius)
            .cursor_pointer()
            .when(chosen, |r| r.bg(active))
            .when(!chosen, |r| r.hover(move |s| s.bg(hover)))
            .aria_label(label)
            .aria_selected(chosen)
            .on_click(
                cx.listener(move |this, _, window, cx| this.activate(item.clone(), window, cx)),
            )
            .child(content)
            .test_support()
            .into_any_element()
    }

    fn render_instance(&self, hit: &InstanceResult, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .child(instance_icon(&hit.icon, ICON, cx))
            .child(primary(hit.name.clone()))
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(TEXT_SECONDARY)
                    .text_color(theme.muted_foreground)
                    .child(hit.detail.clone()),
            )
            .into_any_element()
    }

    fn render_mod(&self, hit: &ModResult, cx: &mut Context<Self>) -> AnyElement {
        let icon = match &hit.hit.icon_path {
            Some(path) => {
                let file = path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let path = path.clone();
                icon_tile(
                    ImageKey::new(INDEX_ICONS, file, 0, None),
                    move || read_icon_file(&path),
                    ICON,
                    cx,
                )
            }
            None => placeholder_icon(Lucide::Puzzle, cx),
        };
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        h_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .child(
                div()
                    .flex_none()
                    .when(!hit.hit.enabled, |d| d.opacity(DISABLED_ICON_OPACITY))
                    .child(icon),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .flex_none()
                            .max_w(rems(12.))
                            .child(primary(hit.hit.name.clone())),
                    )
                    .children(hit.hit.version.clone().map(|v| {
                        div()
                            .flex_none()
                            .max_w(rems(6.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(TEXT_SECONDARY)
                            .text_color(muted)
                            .child(v)
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(TEXT_SECONDARY)
                            .text_color(muted)
                            .child(
                                t!("shell.search.in_instance", instance = hit.instance_name)
                                    .to_string(),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_online(&self, text: String, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .child(placeholder_icon(Lucide::Earth, cx))
            .child(primary(text))
            .into_any_element()
    }
}

/// A row's main text, cut with an ellipsis when it does not fit.
fn primary(text: String) -> impl IntoElement {
    div()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(TEXT_PRIMARY)
        .child(text)
}

/// A muted tile with `icon`, for results without an image.
fn placeholder_icon(icon: Lucide, cx: &mut Context<GlobalSearch>) -> AnyElement {
    let theme = cx.theme();
    h_flex()
        .flex_none()
        .size(ICON)
        .rounded(rems(ICON.0 * 0.22))
        .justify_center()
        .bg(theme.muted)
        .text_color(theme.muted_foreground)
        .child(Icon::new(icon).size(rems(0.75)))
        .into_any_element()
}

/// What a mod row says: "name version — instance".
fn mod_label(hit: &ModResult) -> String {
    let name = match &hit.hit.version {
        Some(version) => format!("{} {version}", hit.hit.name),
        None => hit.hit.name.clone(),
    };
    format!(
        "{name} {}",
        t!("shell.search.in_instance", instance = hit.instance_name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::IconSource;
    use patina_core::content::index::ModHit;

    fn results(instances: usize, mods: Option<usize>) -> SearchResults {
        let mods = match mods {
            None => ModGroup::Indexing,
            Some(n) => ModGroup::Hits(
                (0..n)
                    .map(|i| ModResult {
                        hit: ModHit {
                            instance_id: "i0".into(),
                            file_name: format!("m{i}.jar"),
                            mod_id: None,
                            name: format!("M {i}"),
                            version: None,
                            icon_path: None,
                            enabled: true,
                        },
                        instance_name: "I 0".into(),
                    })
                    .collect(),
            ),
        };
        SearchResults {
            query: "q".into(),
            instances: (0..instances)
                .map(|i| InstanceResult {
                    id: format!("i{i}"),
                    name: format!("I {i}"),
                    detail: String::new(),
                    icon: IconSource::Letter { ch: 'I', tint: 0 },
                })
                .collect(),
            mods,
        }
    }

    #[test]
    fn rows_follow_their_headers() {
        // Header, 2 instances, header, 3 mods, header, online.
        let r = results(2, Some(3));
        assert_eq!(child_index(&r, 0), 1);
        assert_eq!(child_index(&r, 1), 2);
        assert_eq!(child_index(&r, 2), 4);
        assert_eq!(child_index(&r, 5), 8);
        // While indexing, the mods group is a header and a note.
        let r = results(0, None);
        assert_eq!(child_index(&r, 0), 3);
    }
}
