//! 概览 tab: a two-column stats grid (crypto_pro quote panel style) and the
//! instance folder button, separated by a 1px divider rather than boxes.

use std::path::Path;

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Div, InteractiveElement, IntoElement, ParentElement, Rems, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div, rems,
};
use patina_core::content::mods::list_mod_files;
use patina_core::fsutil::{is_safe_name, safe_join};
use patina_core::instance::Instance;
use rust_i18n::t;
use serde::Deserialize;

use super::{date, loader_full, now_secs, time_ago};
use crate::disk_usage::{Walk, format_size};
use crate::layout::{GUTTER, TEXT_PRIMARY, TEXT_SECONDARY, scroll_y};

/// Readable measure of the overview's content; the dividers run full width.
const CONTENT_MAX_W: Rems = Rems(44.);
const GRID_ROW: Rems = Rems(1.625);
/// Most directory entries the disk-use walk of one instance visits.
const MAX_WALK_ENTRIES: usize = 1_000_000;

/// The numbers of the overview that need the disk. `None` where reading
/// failed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct OverviewStats {
    /// Mod files in `mods/`, enabled or not.
    pub mods: Option<usize>,
    /// Folders in `saves/`.
    pub worlds: Option<usize>,
    /// Bytes of the regular files under the instance directory.
    pub size: u64,
    /// False when the walk stopped at its budget: `size` is a lower bound.
    pub size_complete: bool,
    /// The Java major version the instance's Minecraft version asks for,
    /// when that version is installed.
    pub java: Option<u32>,
}

impl OverviewStats {
    /// Reads the stats of `instance`; its Minecraft version's JSON is looked
    /// up under `versions`. Blocking.
    pub fn compute(instance: &Instance, versions: &Path) -> Self {
        Self::compute_with(instance, versions, MAX_WALK_ENTRIES)
    }

    /// The quick fields of `instance` again (mods, worlds, Java) with the
    /// size of `previous`: no walk of the whole directory. Blocking.
    pub fn recount(instance: &Instance, versions: &Path, previous: &OverviewStats) -> Self {
        let game = instance.game_dir();
        Self {
            mods: list_mod_files(&game.join("mods")).map(|m| m.len()).ok(),
            worlds: count_dirs(&game.join("saves")).ok(),
            size: previous.size,
            size_complete: previous.size_complete,
            java: required_java(versions, instance.config.mc_version()),
        }
    }

    fn compute_with(instance: &Instance, versions: &Path, budget: usize) -> Self {
        let mut walk = Walk::new(budget);
        let size_complete = walk.add_tree(&instance.dir);
        let sized = Self {
            size: walk.bytes(),
            size_complete,
            ..Self::default()
        };
        Self::recount(instance, versions, &sized)
    }
}

/// Folders (not links to folders) directly in `dir`; a missing `dir` has
/// none.
fn count_dirs(dir: &Path) -> std::io::Result<usize> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .count()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

/// The Java major version in the installed version JSON of Minecraft `mc`
/// (`versions/<mc>/<mc>.json`), if it is installed. Versions without the
/// field run on Java 8.
pub(crate) fn required_java(versions: &Path, mc: &str) -> Option<u32> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Json {
        java_version: Option<JavaVersion>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct JavaVersion {
        major_version: u32,
    }
    // `mc` comes from the instance's config on disk: one plain name only.
    if !is_safe_name(mc) {
        return None;
    }
    let dir = safe_join(versions, mc)?;
    let text = std::fs::read_to_string(dir.join(format!("{mc}.json"))).ok()?;
    let json: Json = serde_json::from_str(&text).ok()?;
    Some(json.java_version.map_or(8, |j| j.major_version))
}

/// One label / value pair of the grid: muted label left, value right.
fn stat(id: &'static str, label: String, value: String, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .flex_1()
        .min_w_0()
        .justify_between()
        .gap_2()
        .child(
            div()
                .whitespace_nowrap()
                .text_size(TEXT_SECONDARY)
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(
            div()
                .id(id)
                .flex_none()
                .whitespace_nowrap()
                .text_size(TEXT_PRIMARY)
                .text_color(theme.foreground)
                .aria_label(SharedString::from(value.clone()))
                .child(value)
                .test_support(),
        )
}

/// Lays `pairs` out two per row.
fn grid(pairs: Vec<Div>) -> Div {
    let mut grid = v_flex().gap(rems(0.1875));
    let mut it = pairs.into_iter();
    while let Some(a) = it.next() {
        let b = it.next().unwrap_or_else(|| div().flex_1());
        grid = grid.child(h_flex().h(GRID_ROW).gap_10().child(a).child(b));
    }
    grid
}

/// The tab's content for `instance`, with `stats` once they are read.
pub fn render(
    instance: &Instance,
    stats: Option<&OverviewStats>,
    scroll: &ScrollHandle,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let config = &instance.config;
    let pending = || "—".to_string();
    let count = |n: Option<Option<usize>>| n.flatten().map_or_else(pending, |n| n.to_string());
    let size = stats.map_or_else(pending, |s| {
        let size = format_size(s.size);
        if s.size_complete {
            size
        } else {
            t!("instances.overview.size_partial", size = size).to_string()
        }
    });
    let played = config.last_played.map_or_else(
        || t!("instances.time.never").to_string(),
        |t| time_ago(t, now_secs()),
    );
    let pairs = vec![
        stat(
            "stat-loader",
            t!("instances.overview.loader").to_string(),
            loader_full(config),
            cx,
        ),
        stat(
            "stat-version",
            t!("instances.overview.game_version").to_string(),
            config.mc_version().to_string(),
            cx,
        ),
        stat(
            "stat-played",
            t!("instances.overview.last_played").to_string(),
            played,
            cx,
        ),
        stat(
            "stat-created",
            t!("instances.overview.created").to_string(),
            date(config.created_at),
            cx,
        ),
        stat(
            "stat-mods",
            t!("instances.overview.mods").to_string(),
            count(stats.map(|s| s.mods)),
            cx,
        ),
        stat(
            "stat-worlds",
            t!("instances.overview.worlds").to_string(),
            count(stats.map(|s| s.worlds)),
            cx,
        ),
        stat(
            "stat-size",
            t!("instances.overview.size").to_string(),
            size,
            cx,
        ),
    ];
    let dir = instance.dir.clone();
    let content = v_flex()
        .id("overview")
        .child(
            div()
                .w_full()
                .px(GUTTER)
                .pt_3()
                .pb_3()
                .border_b_1()
                .border_color(theme.border)
                .child(v_flex().max_w(CONTENT_MAX_W).child(grid(pairs))),
        )
        .child(
            h_flex().px(GUTTER).py_3().child(
                Button::new("open-instance-folder")
                    .outline()
                    .small()
                    .icon(Icon::new(IconName::FolderOpen))
                    .label(t!("instances.overview.open_folder").to_string())
                    .on_click(move |_, _, cx| cx.open_with_system(&dir)),
            ),
        );
    scroll_y(content, scroll).into_any_element()
}

#[cfg(test)]
mod tests {
    use patina_core::instance::InstanceStore;

    use super::*;

    #[test]
    fn stats_count_mods_worlds_size_and_java() {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path().join("instances"));
        let inst = store.create("Alpha", "1.21.1").unwrap();
        let game = inst.game_dir();
        std::fs::create_dir_all(game.join("mods")).unwrap();
        std::fs::write(game.join("mods/a.jar"), [0u8; 100]).unwrap();
        std::fs::write(game.join("mods/b.jar.disabled"), [0u8; 50]).unwrap();
        std::fs::write(game.join("mods/notes.txt"), "x").unwrap();
        std::fs::create_dir_all(game.join("saves/World 1")).unwrap();
        std::fs::create_dir_all(game.join("saves/World 2")).unwrap();
        std::fs::write(game.join("saves/stray.txt"), "x").unwrap();
        let versions = tmp.path().join("versions");

        let stats = OverviewStats::compute(&inst, &versions);
        assert_eq!(stats.mods, Some(2));
        assert_eq!(stats.worlds, Some(2));
        assert!(stats.size_complete);
        assert!(stats.size >= 152, "{}", stats.size);
        assert_eq!(stats.java, None, "the version is not installed");

        std::fs::create_dir_all(versions.join("1.21.1")).unwrap();
        std::fs::write(
            versions.join("1.21.1/1.21.1.json"),
            r#"{"id":"1.21.1","javaVersion":{"component":"java-runtime-delta","majorVersion":21}}"#,
        )
        .unwrap();
        assert_eq!(OverviewStats::compute(&inst, &versions).java, Some(21));
        std::fs::write(versions.join("1.21.1/1.21.1.json"), r#"{"id":"1.21.1"}"#).unwrap();
        assert_eq!(OverviewStats::compute(&inst, &versions).java, Some(8));

        let cut = OverviewStats::compute_with(&inst, &versions, 3);
        assert!(!cut.size_complete);

        // A recount keeps the size and counts again.
        let before = OverviewStats::compute(&inst, &versions);
        std::fs::write(game.join("mods/c.jar"), [0u8; 4096]).unwrap();
        let recounted = OverviewStats::recount(&inst, &versions, &before);
        assert_eq!(recounted.mods, Some(3));
        assert_eq!(
            (recounted.size, recounted.size_complete),
            (before.size, true)
        );
    }

    #[test]
    fn a_version_name_cannot_leave_the_versions_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let versions = tmp.path().join("versions");
        std::fs::create_dir_all(&versions).unwrap();
        std::fs::write(
            tmp.path().join("...json"),
            r#"{"javaVersion":{"majorVersion":99}}"#,
        )
        .unwrap();
        assert_eq!(required_java(&versions, ".."), None);
        assert_eq!(required_java(&versions, "../x"), None);
    }
}
