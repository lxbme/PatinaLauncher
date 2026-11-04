//! Modrinth v2 mapping. Only hash identifiers are submitted for local identification.
use super::{client::*, *};
use serde_json::{Value, json};

pub(super) fn kind_name(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Mod => "mod",
        ContentKind::ResourcePack => "resourcepack",
        ContentKind::Shader => "shader",
        ContentKind::Modpack => "modpack",
    }
}
fn kind(value: &str) -> Result<ContentKind, OnlineError> {
    match value {
        "mod" => Ok(ContentKind::Mod),
        "resourcepack" => Ok(ContentKind::ResourcePack),
        "shader" => Ok(ContentKind::Shader),
        "modpack" => Ok(ContentKind::Modpack),
        _ => Err(OnlineError::InvalidResponse),
    }
}
fn client_supported(value: &Value) -> bool {
    !value
        .get("environment")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|v| {
                matches!(
                    v.as_str(),
                    Some("server_only" | "server_only_client_optional" | "dedicated_server_only")
                )
            })
        })
        && value.get("client_side").and_then(Value::as_str) != Some("unsupported")
}
pub(super) async fn search(api: ApiClient, q: OnlineQuery) -> Result<OnlinePage, OnlineError> {
    let mut facets = vec![vec![format!("project_type:{}", kind_name(q.kind))]];
    if let Some(version) = q.mc_version {
        facets.push(vec![format!("versions:{version}")]);
    }
    if q.kind == ContentKind::Mod
        && let Some(loader) = q.loader
    {
        facets.push(vec![format!("categories:{}", loader_name(loader))]);
    }
    if let Some(category) = q.category {
        facets.push(vec![format!("categories:{category}")]);
    }
    let data = api
        .get(
            &["search"],
            vec![
                ("query".into(), q.text),
                ("facets".into(), json!(facets).to_string()),
                (
                    "index".into(),
                    match q.sort {
                        SortOrder::Relevance => "relevance",
                        SortOrder::Downloads => "downloads",
                        SortOrder::Updated => "updated",
                    }
                    .into(),
                ),
                ("offset".into(), q.offset.to_string()),
                ("limit".into(), q.limit.clamp(1, 100).to_string()),
            ],
        )
        .await?;
    let hits = array(&data, "hits")?
        .iter()
        .map(|v| {
            Ok(OnlineHit {
                id: text(v, "project_id")?.into(),
                title: text(v, "title")?.into(),
                author: text(v, "author")?.into(),
                summary: text(v, "description")?.into(),
                downloads: v["downloads"].as_u64().unwrap_or(0),
                updated: timestamp(v, "date_modified"),
                icon_url: v["icon_url"].as_str().map(str::to_owned),
                tags: strings(v, "categories"),
            })
        })
        .collect::<Result<Vec<_>, OnlineError>>()?;
    Ok(OnlinePage {
        hits,
        total: data["total_hits"]
            .as_u64()
            .ok_or(OnlineError::InvalidResponse)?,
    })
}
pub(super) async fn project(
    api: ApiClient,
    reference: ProjectRef,
) -> Result<ContentProject, OnlineError> {
    let data = api.get(&["project", reference.id()], vec![]).await?;
    if text(&data, "id")? != reference.id() {
        return Err(OnlineError::InvalidResponse);
    }
    let kind = kind(text(&data, "project_type")?)?;
    let team = api
        .get(&["team", text(&data, "team")?, "members"], vec![])
        .await?;
    let authors = team
        .as_array()
        .ok_or(OnlineError::InvalidResponse)?
        .iter()
        .filter_map(|v| v["user"]["username"].as_str().map(str::to_owned))
        .collect();
    Ok(ContentProject {
        reference,
        kind,
        title: text(&data, "title")?.into(),
        description: text(&data, "body")?.into(),
        authors,
        url: format!(
            "https://modrinth.com/{}/{}",
            kind_name(kind),
            text(&data, "slug")?
        ),
        support_urls: data["donation_urls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["url"].as_str().map(str::to_owned))
            .collect(),
        client_supported: client_supported(&data),
        icon_url: data["icon_url"].as_str().map(str::to_owned),
    })
}
pub(super) async fn versions(
    api: ApiClient,
    project: ProjectRef,
    target: &TargetCompatibility,
) -> Result<Vec<ContentVersion>, OnlineError> {
    // Project-side availability applies to every version, including explicit dependencies.
    if !self::project(api.clone(), project.clone())
        .await?
        .is_client_supported()
    {
        return Ok(vec![]);
    }
    let mut query = vec![(
        "game_versions".into(),
        json!([target.mc_version()]).to_string(),
    )];
    if let Some(loader) = target.loader() {
        query.push(("loaders".into(), json!([loader_name(loader)]).to_string()));
    }
    let data = api
        .get(&["project", project.id(), "version"], query)
        .await?;
    let versions = data
        .as_array()
        .ok_or(OnlineError::InvalidResponse)?
        .iter()
        .map(parse_version)
        .collect::<Result<Vec<_>, _>>()?;
    if versions.iter().any(|v| v.reference().project() != &project) {
        return Err(OnlineError::InvalidResponse);
    }
    Ok(versions)
}
pub(super) async fn pack_versions(
    api: ApiClient,
    project: ProjectRef,
) -> Result<Vec<ContentVersion>, OnlineError> {
    let metadata = self::project(api.clone(), project.clone()).await?;
    if metadata.kind() != ContentKind::Modpack || !metadata.is_client_supported() {
        return Err(OnlineError::InvalidResponse);
    }
    let data = api
        .get(&["project", project.id(), "version"], vec![])
        .await?;
    let mut versions = data
        .as_array()
        .ok_or(OnlineError::InvalidResponse)?
        .iter()
        .map(parse_version)
        .collect::<Result<Vec<_>, _>>()?;
    if versions.iter().any(|v| v.reference().project() != &project) {
        return Err(OnlineError::InvalidResponse);
    }
    versions.sort_by_key(|v| std::cmp::Reverse(v.published()));
    Ok(versions)
}
pub(super) async fn version(api: ApiClient, id: &str) -> Result<ContentVersion, OnlineError> {
    parse_version(&api.get(&["version", id], vec![]).await?)
}
fn parse_version(data: &Value) -> Result<ContentVersion, OnlineError> {
    let project = ProjectRef::new(Platform::Modrinth, text(data, "project_id")?);
    let reference = VersionRef::new(project.clone(), text(data, "id")?);
    let dependencies = array(data, "dependencies")?
        .iter()
        .map(|d| {
            let project = d["project_id"]
                .as_str()
                .map(|id| ProjectRef::new(Platform::Modrinth, id));
            // Explicit version IDs can omit project_id; the version endpoint resolves identity.
            let version = d["version_id"].as_str().map(str::to_owned);
            Ok(Dependency::new(
                project,
                version,
                match text(d, "dependency_type")? {
                    "required" => DependencyRelation::Required,
                    "optional" => DependencyRelation::Optional,
                    "incompatible" => DependencyRelation::Incompatible,
                    "embedded" => DependencyRelation::Embedded,
                    _ => return Err(OnlineError::InvalidResponse),
                },
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let page_url = format!(
        "https://modrinth.com/project/{}/version/{}",
        project.id(),
        reference.id()
    );
    let files = array(data, "files")?
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            !matches!(
                f["file_type"].as_str(),
                Some("required-resource-pack" | "optional-resource-pack")
            )
        })
        .map(|(ix, f)| {
            Ok(ContentFile {
                name: text(f, "filename")?.into(),
                size: f["size"].as_u64().ok_or(OnlineError::InvalidResponse)?,
                hashes: FileHashes::new(
                    f["hashes"]["sha1"].as_str().map(str::to_owned),
                    f["hashes"]["sha512"].as_str().map(str::to_owned),
                    None,
                ),
                download_url: f["url"].as_str().map(str::to_owned),
                page_url: page_url.clone(),
                primary: f["primary"].as_bool().unwrap_or(ix == 0),
            })
        })
        .collect::<Result<Vec<_>, OnlineError>>()?;
    Ok(ContentVersion {
        reference,
        name: text(data, "name")?.into(),
        channel: match text(data, "version_type")? {
            "release" => ReleaseChannel::Release,
            "beta" => ReleaseChannel::Beta,
            "alpha" => ReleaseChannel::Alpha,
            _ => return Err(OnlineError::InvalidResponse),
        },
        published: timestamp(data, "date_published"),
        game_versions: strings(data, "game_versions"),
        loaders: strings(data, "loaders")
            .iter()
            .filter_map(|s| loader(s))
            .collect(),
        files,
        dependencies,
    })
}
pub(super) async fn identify(
    api: ApiClient,
    files: Vec<FileIdentity>,
) -> Result<Vec<IdentifiedFile>, OnlineError> {
    let mut found = serde_json::Map::new();
    for chunk in files.chunks(100) {
        let data=api.request(reqwest::Method::POST,&["version_files"],&[],Some(json!({"hashes":chunk.iter().map(FileIdentity::sha1).collect::<Vec<_>>(),"algorithm":"sha1"}))).await?;
        found.extend(
            data.as_object()
                .ok_or(OnlineError::InvalidResponse)?
                .clone(),
        );
    }
    files
        .into_iter()
        .map(|identity| {
            let matches = found
                .get(identity.sha1())
                .map(parse_version)
                .transpose()?
                .into_iter()
                .collect();
            Ok(IdentifiedFile { identity, matches })
        })
        .collect()
}
pub(super) async fn categories(
    api: ApiClient,
    kind: ContentKind,
) -> Result<Vec<ContentCategory>, OnlineError> {
    let data = api.get(&["tag", "category"], vec![]).await?;
    data.as_array()
        .ok_or(OnlineError::InvalidResponse)?
        .iter()
        .filter(|v| v["project_type"].as_str() == Some(kind_name(kind)))
        .map(|v| Ok(ContentCategory::new(text(v, "name")?, text(v, "name")?)))
        .collect()
}
