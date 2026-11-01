//! Compact installed-source links. Identity comes only from the instance's provenance records.
use crate::layout::TEXT_PRIMARY;
use gpui_kit::base::Link;
use gpui_kit::component::{ActiveTheme, tooltip::Tooltip};
use gpui_kit::{
    AnyElement, App, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, actions, div, rems,
};
use patina_core::content::online::{ContentKind, Platform, ProjectRef, load_sources};
use patina_core::instance::Instance;
use rust_i18n::t;
use std::collections::HashMap;
use std::path::PathBuf;

actions!(content, [OpenOriginalProject]);

pub(super) fn load_origins(instance: &Instance) -> HashMap<PathBuf, ProjectRef> {
    let sources = match load_sources(&instance.dir) {
        Ok(sources) => sources,
        Err(error) => {
            // Decoration must not hide usable files; content mutations still reject corrupt records.
            tracing::warn!(%error,"could not load installed content origins");
            return HashMap::new();
        }
    };
    let game = instance.game_dir();
    let mut origins = HashMap::new();
    for record in sources.records() {
        let project = record.version().project();
        if project_url(project).is_none() {
            continue;
        }
        let mut relative = record.path().to_string();
        let path = game.join(&relative);
        if record.kind() == ContentKind::Mod && !path.exists() {
            let alternate = relative
                .strip_suffix(".disabled")
                .map_or_else(|| format!("{relative}.disabled"), str::to_owned);
            if !sources.records().iter().any(|r| r.path() == alternate)
                && game.join(&alternate).is_file()
            {
                relative = alternate;
            }
        }
        let path = game.join(relative);
        if path.is_file() {
            origins.insert(path, project.clone());
        }
    }
    origins
}

/// Stable platform ID routes avoid guessing a project slug or fetching metadata just to show a row.
fn project_url(project: &ProjectRef) -> Option<String> {
    let id = project.id();
    if id.is_empty() {
        return None;
    }
    match project.platform() {
        Platform::Modrinth
            if id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) =>
        {
            Some(format!("https://modrinth.com/project/{id}"))
        }
        Platform::CurseForge if id.bytes().all(|b| b.is_ascii_digit()) => {
            Some(format!("https://www.curseforge.com/projects/{id}"))
        }
        _ => None,
    }
}
pub(super) fn open_project(project: Option<&ProjectRef>, cx: &mut App) {
    if let Some(url) = project.and_then(project_url) {
        cx.open_url(&url);
    }
}

pub(super) fn origin_badge(relative: &str, project: Option<&ProjectRef>, cx: &App) -> AnyElement {
    let (muted, foreground, background) = (
        cx.theme().muted_foreground,
        cx.theme().foreground,
        cx.theme().muted,
    );
    let label = match project.map(ProjectRef::platform) {
        Some(Platform::Modrinth) => t!("downloads.platform.modrinth").to_string(),
        Some(Platform::CurseForge) => t!("downloads.platform.curseforge").to_string(),
        None => t!("content.origin.local").to_string(),
    };
    let badge = div()
        .id(SharedString::from(format!("content-origin:{relative}")))
        .aria_label(label.clone())
        .test_support()
        .flex_none()
        .w(rems(6.5))
        .text_size(TEXT_PRIMARY)
        .text_color(muted);
    if let Some(project) = project.filter(|p| project_url(p).is_some()) {
        let url = project_url(project).expect("validated project identity");
        badge
            .child(
                Link::new(SharedString::from(format!(
                    "content-origin-link:{relative}"
                )))
                .href(url)
                .tab_stop(false)
                .accessibility_label(t!("content.origin.open", platform = &label).to_string())
                .rounded_sm()
                .px_1p5()
                .py_0p5()
                .bg(background)
                .text_color(muted)
                .hover(move |s| s.text_color(foreground))
                .cursor_pointer()
                .tooltip(|window, cx| {
                    Tooltip::new(t!("content.origin.shortcut").to_string()).build(window, cx)
                })
                .open_with(|url, _, _, cx| cx.open_url(url))
                .on_activate(|_, _, cx| cx.stop_propagation())
                .child(label),
            )
            .into_any_element()
    } else {
        badge.child(label).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_urls_use_platform_identity_not_untrusted_path_text() {
        assert_eq!(
            project_url(&ProjectRef::new(Platform::Modrinth, "AANobbMI")).as_deref(),
            Some("https://modrinth.com/project/AANobbMI")
        );
        assert_eq!(
            project_url(&ProjectRef::new(Platform::CurseForge, "123")).as_deref(),
            Some("https://www.curseforge.com/projects/123")
        );
        assert!(project_url(&ProjectRef::new(Platform::CurseForge, "../../elsewhere")).is_none());
        assert!(
            project_url(&ProjectRef::new(Platform::Modrinth, "a?redirect=elsewhere")).is_none()
        );
    }
}
