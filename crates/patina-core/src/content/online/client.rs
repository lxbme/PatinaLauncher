//! Bounded metadata HTTP client. CF credentials are attached only to CF API requests.
use super::*;
use futures::{StreamExt, future::BoxFuture};
use reqwest::{Client, Method, Url};
use serde_json::Value;
use std::time::Duration;

const MAX_METADATA: usize = 16 * 1024 * 1024;
#[derive(Clone)]
pub(super) struct ApiClient {
    client: Client,
    base: Url,
    key: Option<String>,
    requires_key: bool,
}
impl ApiClient {
    fn new(base: &str, key: Option<String>, requires_key: bool) -> Result<Self, OnlineError> {
        let base = Url::parse(base).map_err(|_| OnlineError::InvalidResponse)?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(OnlineError::InvalidResponse);
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!(
                "PatinaLauncher/",
                env!("CARGO_PKG_VERSION"),
                " (desktop Minecraft launcher)"
            ))
            // Never forward the application header to a redirect target, including same-origin downloads.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| OnlineError::Unavailable)?;
        Ok(Self {
            client,
            base,
            key: key.filter(|s| !s.trim().is_empty()),
            requires_key,
        })
    }
    pub(super) async fn request(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value, OnlineError> {
        if self.requires_key && self.key.is_none() {
            return Err(OnlineError::MissingApiKey);
        }
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| OnlineError::InvalidResponse)?;
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let mut request = self.client.request(method, url);
        if let Some(key) = &self.key {
            request = request.header("x-api-key", key);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        // Don't include request URLs/headers or response bodies in errors.
        let response = request.send().await.map_err(|e| {
            OnlineError::Network(
                if e.is_timeout() {
                    "timeout"
                } else {
                    "connection failed"
                }
                .into(),
            )
        })?;
        let status = response.status();
        if status.as_u16() == 429 {
            return Err(OnlineError::RateLimited(
                response
                    .headers()
                    .get("Retry-After")
                    .and_then(|s| s.to_str().ok())
                    .and_then(|s| s.parse().ok()),
            ));
        }
        if status.as_u16() == 404 {
            return Err(OnlineError::NotFound);
        }
        if !status.is_success() {
            return Err(OnlineError::Network(format!("HTTP {}", status.as_u16())));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_METADATA as u64)
        {
            return Err(OnlineError::InvalidResponse);
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| OnlineError::Network("response interrupted".into()))?;
            if bytes.len().saturating_add(chunk.len()) > MAX_METADATA {
                return Err(OnlineError::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| OnlineError::InvalidResponse)
    }
    pub(super) async fn get(
        &self,
        segments: &[&str],
        query: Vec<(String, String)>,
    ) -> Result<Value, OnlineError> {
        self.request(Method::GET, segments, &query, None).await
    }
}

/// Routes each identity to its own platform; no cross-platform replacement or fallback.
#[derive(Clone)]
pub struct ContentCatalogs {
    modrinth: ApiClient,
    curseforge: ApiClient,
}
impl ContentCatalogs {
    pub fn new() -> Result<Self, OnlineError> {
        Self::with_endpoints(
            "https://api.modrinth.com/v2",
            "https://api.curseforge.com/v1",
            option_env!("PATINA_CURSEFORGE_API_KEY").map(str::to_owned),
        )
    }
    /// Explicit endpoints support isolated loopback API contract tests.
    pub fn with_endpoints(
        modrinth: &str,
        curseforge: &str,
        key: Option<String>,
    ) -> Result<Self, OnlineError> {
        Ok(Self {
            modrinth: ApiClient::new(modrinth, None, false)?,
            curseforge: ApiClient::new(curseforge, key, true)?,
        })
    }
    fn api(&self, platform: Platform) -> ApiClient {
        match platform {
            Platform::Modrinth => self.modrinth.clone(),
            Platform::CurseForge => self.curseforge.clone(),
        }
    }
}
impl ContentSource for ContentCatalogs {
    fn search(&self, query: OnlineQuery) -> BoxFuture<'static, Result<OnlinePage, OnlineError>> {
        let api = self.api(query.platform);
        Box::pin(async move {
            match query.platform {
                Platform::Modrinth => super::modrinth::search(api, query).await,
                Platform::CurseForge => super::curseforge::search(api, query).await,
            }
        })
    }
    fn project(
        &self,
        project: ProjectRef,
    ) -> BoxFuture<'static, Result<ContentProject, OnlineError>> {
        let api = self.api(project.platform());
        Box::pin(async move {
            match project.platform() {
                Platform::Modrinth => super::modrinth::project(api, project).await,
                Platform::CurseForge => super::curseforge::project(api, project).await,
            }
        })
    }
    fn versions(
        &self,
        project: ProjectRef,
        target: TargetCompatibility,
    ) -> BoxFuture<'static, Result<Vec<ContentVersion>, OnlineError>> {
        let api = self.api(project.platform());
        Box::pin(async move {
            let mut versions = match project.platform() {
                Platform::Modrinth => super::modrinth::versions(api, project, &target).await?,
                Platform::CurseForge => super::curseforge::versions(api, project, &target).await?,
            };
            versions.retain(|v| target.accepts(v));
            versions.sort_by_key(|v| std::cmp::Reverse(v.published()));
            Ok(versions)
        })
    }
    fn pack_versions(
        &self,
        project: ProjectRef,
    ) -> BoxFuture<'static, Result<Vec<ContentVersion>, OnlineError>> {
        let api = self.api(project.platform());
        Box::pin(async move {
            if project.platform() != Platform::Modrinth {
                return Err(OnlineError::Unavailable);
            }
            super::modrinth::pack_versions(api, project).await
        })
    }
    fn version(
        &self,
        platform: Platform,
        id: String,
    ) -> BoxFuture<'static, Result<ContentVersion, OnlineError>> {
        let api = self.api(platform);
        Box::pin(async move {
            let result = match platform {
                Platform::Modrinth => super::modrinth::version(api, &id).await?,
                Platform::CurseForge => super::curseforge::version(api, &id).await?,
            };
            if result.reference().id() != id {
                return Err(OnlineError::InvalidResponse);
            }
            Ok(result)
        })
    }
    fn identify(
        &self,
        platform: Platform,
        files: Vec<FileIdentity>,
    ) -> BoxFuture<'static, Result<Vec<IdentifiedFile>, OnlineError>> {
        let api = self.api(platform);
        Box::pin(async move {
            if files.is_empty() {
                return Ok(vec![]);
            }
            match platform {
                Platform::Modrinth => super::modrinth::identify(api, files).await,
                Platform::CurseForge => super::curseforge::identify(api, files).await,
            }
        })
    }
    fn categories(
        &self,
        platform: Platform,
        kind: ContentKind,
    ) -> BoxFuture<'static, Result<Vec<ContentCategory>, OnlineError>> {
        let api = self.api(platform);
        Box::pin(async move {
            match platform {
                Platform::Modrinth => super::modrinth::categories(api, kind).await,
                Platform::CurseForge => super::curseforge::categories(api, kind).await,
            }
        })
    }
}
pub(super) fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str, OnlineError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(OnlineError::InvalidResponse)
}
pub(super) fn array<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>, OnlineError> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or(OnlineError::InvalidResponse)
}
pub(super) fn strings(value: &Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
pub(super) fn timestamp(value: &Value, field: &str) -> u64 {
    value
        .get(field)
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp().max(0) as u64)
        .unwrap_or(0)
}
pub(super) fn loader_name(loader: crate::loader::LoaderKind) -> &'static str {
    match loader {
        crate::loader::LoaderKind::Forge => "forge",
        crate::loader::LoaderKind::Neoforge => "neoforge",
        crate::loader::LoaderKind::Fabric => "fabric",
    }
}
pub(super) fn loader(value: &str) -> Option<crate::loader::LoaderKind> {
    match value.to_ascii_lowercase().as_str() {
        "forge" => Some(crate::loader::LoaderKind::Forge),
        "neoforge" => Some(crate::loader::LoaderKind::Neoforge),
        "fabric" => Some(crate::loader::LoaderKind::Fabric),
        _ => None,
    }
}
