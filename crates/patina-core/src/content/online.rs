//! Online content catalog seam and platform-independent metadata.

mod client;
mod curseforge;
mod installed;
mod model;
mod plan;
mod service;
mod updates;
pub(crate) use installed::record_pack_files;
pub use installed::{ContentSources, SourceRecord, load_sources};
pub use plan::*;
pub use service::{ContentError, ContentService};
pub use updates::{ContentUpdate, ContentUpdatePlan, UpdateSelection};
mod modrinth;
pub use client::ContentCatalogs;
pub use model::*;
use serde::{Deserialize, Serialize};

use crate::loader::LoaderKind;
use futures::future::BoxFuture;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContentKind {
    Mod,
    ResourcePack,
    Shader,
    Modpack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Platform {
    Modrinth,
    CurseForge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortOrder {
    Relevance,
    Downloads,
    Updated,
}

/// Paging rule: the source returns at most `limit` hits starting at `offset`;
/// the next page starts at `offset + hits.len()`. `OnlinePage::total` is the
/// platform's total number of matches, not the size of this page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlineQuery {
    pub kind: ContentKind,
    pub text: String,
    pub platform: Platform,
    pub mc_version: Option<String>,
    pub loader: Option<LoaderKind>,
    pub category: Option<String>,
    pub sort: SortOrder,
    pub offset: u32,
    /// Page size: the most hits one response may carry.
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlineHit {
    pub id: String,
    pub title: String,
    pub author: String,
    pub summary: String,
    pub downloads: u64,
    /// Last update, seconds since the Unix epoch.
    pub updated: u64,
    pub icon_url: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlinePage {
    pub hits: Vec<OnlineHit>,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OnlineError {
    #[error("{}", rust_i18n::t!("errors.online.unavailable"))]
    Unavailable,
    #[error("{}", rust_i18n::t!("errors.online.network", error = .0))]
    Network(String),
    #[error("{}", rust_i18n::t!("errors.online.missing_key"))]
    MissingApiKey,
    #[error("{}", rust_i18n::t!("errors.online.rate_limited"))]
    RateLimited(Option<u64>),
    #[error("{}", rust_i18n::t!("errors.online.invalid_response"))]
    InvalidResponse,
    #[error("{}", rust_i18n::t!("errors.online.not_found"))]
    NotFound,
}

pub trait ContentSource: Send + Sync {
    fn search(&self, q: OnlineQuery) -> BoxFuture<'static, Result<OnlinePage, OnlineError>>;
    fn project(
        &self,
        _project: ProjectRef,
    ) -> BoxFuture<'static, Result<ContentProject, OnlineError>> {
        Box::pin(async { Err(OnlineError::Unavailable) })
    }
    fn versions(
        &self,
        _project: ProjectRef,
        _target: TargetCompatibility,
    ) -> BoxFuture<'static, Result<Vec<ContentVersion>, OnlineError>> {
        Box::pin(async { Err(OnlineError::Unavailable) })
    }
    /// Pack versions are not filtered against the current instance's components.
    fn pack_versions(
        &self,
        _project: ProjectRef,
    ) -> BoxFuture<'static, Result<Vec<ContentVersion>, OnlineError>> {
        Box::pin(async { Err(OnlineError::Unavailable) })
    }
    fn version(
        &self,
        _platform: Platform,
        _id: String,
    ) -> BoxFuture<'static, Result<ContentVersion, OnlineError>> {
        Box::pin(async { Err(OnlineError::Unavailable) })
    }
    /// Platform must be explicit: identities contain fingerprints, not a guessed source.
    fn identify(
        &self,
        _platform: Platform,
        _files: Vec<FileIdentity>,
    ) -> BoxFuture<'static, Result<Vec<IdentifiedFile>, OnlineError>> {
        Box::pin(async { Err(OnlineError::Unavailable) })
    }
    fn categories(
        &self,
        _platform: Platform,
        _kind: ContentKind,
    ) -> BoxFuture<'static, Result<Vec<ContentCategory>, OnlineError>> {
        Box::pin(async { Err(OnlineError::Unavailable) })
    }
}

/// Stand-in used until a real source exists: always unavailable.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableSource;

impl ContentSource for UnavailableSource {
    fn search(&self, _q: OnlineQuery) -> BoxFuture<'static, Result<OnlinePage, OnlineError>> {
        Box::pin(async { Err(OnlineError::Unavailable) })
    }
}

const _: fn() = || {
    fn assert_obj(_: std::sync::Arc<dyn ContentSource>) {}
    assert_obj(std::sync::Arc::new(UnavailableSource));
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Language, set_language};

    fn query() -> OnlineQuery {
        OnlineQuery {
            kind: ContentKind::Mod,
            text: "sodium".into(),
            platform: Platform::Modrinth,
            mc_version: None,
            loader: None,
            category: None,
            sort: SortOrder::Relevance,
            offset: 0,
            limit: 20,
        }
    }

    #[tokio::test]
    async fn unavailable_source_reports_unavailable() {
        let src: std::sync::Arc<dyn ContentSource> = std::sync::Arc::new(UnavailableSource);
        assert_eq!(src.search(query()).await, Err(OnlineError::Unavailable));
    }

    #[test]
    fn online_errors_localized() {
        let _locale = crate::i18n::locale_lock();
        let msgs = |lang| {
            set_language(lang);
            [
                OnlineError::Unavailable.to_string(),
                OnlineError::Network("boom".into()).to_string(),
            ]
        };
        let zh = msgs(Language::ZhCn);
        let en = msgs(Language::En);
        set_language(Language::ZhCn);
        for (z, e) in zh.iter().zip(&en) {
            assert!(
                z.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{z}"
            );
            assert!(e.is_ascii() && !e.starts_with("errors."), "{e}");
            assert!(!z.starts_with("errors."), "{z}");
        }
        assert!(zh[1].contains("boom") && en[1].contains("boom"));
    }
}
