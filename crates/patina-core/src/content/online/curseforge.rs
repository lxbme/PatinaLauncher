//! CurseForge Core API mapping. A missing/null download URL stays restricted.
use super::{client::*, *};
use serde_json::{Value, json};

fn id(value: &Value, field: &str) -> Result<String, OnlineError> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .ok_or(OnlineError::InvalidResponse)
}
fn class_kind(value: &Value) -> Option<ContentKind> {
    if value["isClass"].as_bool() != Some(true) {
        return None;
    }
    match value["slug"].as_str()? {
        "mc-mods" => Some(ContentKind::Mod),
        "texture-packs" => Some(ContentKind::ResourcePack),
        "shaders" => Some(ContentKind::Shader),
        "modpacks" => Some(ContentKind::Modpack),
        _ => None,
    }
}
async fn metadata(api: &ApiClient) -> Result<Value, OnlineError> {
    api.get(&["categories"], vec![("gameId".into(), "432".into())])
        .await
}
fn find_class(metadata: &Value, kind: ContentKind) -> Result<String, OnlineError> {
    array(metadata, "data")?
        .iter()
        .find(|v| class_kind(v) == Some(kind))
        .ok_or(OnlineError::Unavailable)
        .and_then(|v| id(v, "id"))
}
fn loader_number(value: crate::loader::LoaderKind) -> &'static str {
    match value {
        crate::loader::LoaderKind::Forge => "1",
        crate::loader::LoaderKind::Fabric => "4",
        crate::loader::LoaderKind::Neoforge => "6",
    }
}
pub(super) async fn search(api: ApiClient, q: OnlineQuery) -> Result<OnlinePage, OnlineError> {
    let class = find_class(&metadata(&api).await?, q.kind)?;
    // The documented search window is index + pageSize <= 10,000.
    if q.offset >= 10_000 {
        return Ok(OnlinePage {
            hits: vec![],
            total: 10_000,
        });
    }
    let mut query = vec![
        ("gameId".into(), "432".into()),
        ("classId".into(), class),
        ("searchFilter".into(), q.text),
        ("index".into(), q.offset.to_string()),
        (
            "pageSize".into(),
            q.limit.clamp(1, 50).min(10_000 - q.offset).to_string(),
        ),
        (
            "sortField".into(),
            match q.sort {
                SortOrder::Relevance => "1",
                SortOrder::Downloads => "6",
                SortOrder::Updated => "3",
            }
            .into(),
        ),
        ("sortOrder".into(), "desc".into()),
    ];
    if let Some(version) = q.mc_version {
        query.push(("gameVersion".into(), version));
        if q.kind == ContentKind::Mod
            && let Some(loader) = q.loader
        {
            query.push(("modLoaderType".into(), loader_number(loader).into()));
        }
    }
    if let Some(category) = q.category {
        query.push(("categoryId".into(), category));
    }
    let data = api.get(&["mods", "search"], query).await?;
    let hits = array(&data, "data")?
        .iter()
        .map(|v| {
            Ok(OnlineHit {
                id: id(v, "id")?,
                title: text(v, "name")?.into(),
                author: v["authors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a["name"].as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                summary: text(v, "summary")?.into(),
                downloads: v["downloadCount"].as_f64().unwrap_or(0.) as u64,
                updated: timestamp(v, "dateModified"),
                icon_url: v["logo"]["thumbnailUrl"].as_str().map(str::to_owned),
                tags: v["categories"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["name"].as_str().map(str::to_owned))
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>, OnlineError>>()?;
    Ok(OnlinePage {
        hits,
        total: data["pagination"]["totalCount"]
            .as_u64()
            .ok_or(OnlineError::InvalidResponse)?,
    })
}
pub(super) async fn project(
    api: ApiClient,
    reference: ProjectRef,
) -> Result<ContentProject, OnlineError> {
    let data = api.get(&["mods", reference.id()], vec![]).await?;
    let data = &data["data"];
    if id(data, "id")? != reference.id() {
        return Err(OnlineError::InvalidResponse);
    }
    let classes = metadata(&api).await?;
    let kind = array(&classes, "data")?
        .iter()
        .find(|c| c["id"] == data["classId"])
        .and_then(class_kind)
        .ok_or(OnlineError::Unavailable)?;
    let description = api
        .get(&["mods", reference.id(), "description"], vec![])
        .await?;
    Ok(ContentProject {
        reference,
        kind,
        title: text(data, "name")?.into(),
        description: text(&description, "data")?.into(),
        authors: data["authors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["name"].as_str().map(str::to_owned))
            .collect(),
        url: text(&data["links"], "websiteUrl")?.into(),
        support_urls: ["issuesUrl", "sourceUrl", "wikiUrl"]
            .iter()
            .filter_map(|k| data["links"][k].as_str().map(str::to_owned))
            .collect(),
        client_supported: true,
        icon_url: None,
    })
}
async fn project_downloads(api: &ApiClient, project: &str) -> Result<(String, bool), OnlineError> {
    let response = api.get(&["mods", project], vec![]).await?;
    let data = &response["data"];
    Ok((
        text(&data["links"], "websiteUrl")?
            .trim_end_matches('/')
            .to_string(),
        data["allowModDistribution"].as_bool() != Some(false),
    ))
}
pub(super) async fn versions(
    api: ApiClient,
    project: ProjectRef,
    target: &TargetCompatibility,
) -> Result<Vec<ContentVersion>, OnlineError> {
    let (page, allowed) = project_downloads(&api, project.id()).await?;
    let mut output = vec![];
    let mut index = 0;
    loop {
        let mut query = vec![
            ("gameVersion".into(), target.mc_version().into()),
            ("index".into(), index.to_string()),
            ("pageSize".into(), "50".into()),
        ];
        if let Some(loader) = target.loader() {
            query.push(("modLoaderType".into(), loader_number(loader).into()));
        }
        let data = api.get(&["mods", project.id(), "files"], query).await?;
        let files = array(&data, "data")?;
        for file in files {
            if file["isServerPack"].as_bool() == Some(true)
                || file["isAvailable"].as_bool() == Some(false)
            {
                continue;
            }
            let version = parse_file(file, &page, allowed)?;
            if version.reference().project() != &project {
                return Err(OnlineError::InvalidResponse);
            }
            output.push(version);
        }
        index += files.len();
        if files.is_empty()
            || index
                >= data["pagination"]["totalCount"]
                    .as_u64()
                    .unwrap_or(index as u64) as usize
        {
            break;
        }
        // Ordinary bounded browsing, never an unbounded catalog crawl.
        if index >= 10_000 {
            return Err(OnlineError::InvalidResponse);
        }
    }
    Ok(output)
}
pub(super) async fn version(api: ApiClient, file_id: &str) -> Result<ContentVersion, OnlineError> {
    let file_id = file_id
        .parse::<u64>()
        .map_err(|_| OnlineError::InvalidResponse)?;
    let data = api
        .request(
            reqwest::Method::POST,
            &["mods", "files"],
            &[],
            Some(json!({"fileIds":[file_id]})),
        )
        .await?;
    let file = array(&data, "data")?.first().ok_or(OnlineError::NotFound)?;
    if file["isServerPack"].as_bool() == Some(true) || file["isAvailable"].as_bool() == Some(false)
    {
        return Err(OnlineError::Unavailable);
    }
    let (page, allowed) = project_downloads(&api, &id(file, "modId")?).await?;
    parse_file(file, &page, allowed)
}
fn parse_file(data: &Value, page: &str, allowed: bool) -> Result<ContentVersion, OnlineError> {
    let project = ProjectRef::new(Platform::CurseForge, id(data, "modId")?);
    let reference = VersionRef::new(project.clone(), id(data, "id")?);
    let hashes = array(data, "hashes")?;
    let hash = |algo| {
        hashes
            .iter()
            .find(|h| h["algo"].as_u64() == Some(algo))
            .and_then(|h| h["value"].as_str())
            .map(str::to_owned)
    };
    let file = ContentFile::new(
        text(data, "fileName")?,
        data["fileLength"]
            .as_u64()
            .ok_or(OnlineError::InvalidResponse)?,
        FileHashes::new(hash(1), None, hash(2)),
        data["downloadUrl"]
            .as_str()
            .filter(|s| allowed && !s.is_empty())
            .map(str::to_owned),
        format!("{page}/files/{}", reference.id()),
    );
    let dependencies = array(data, "dependencies")?
        .iter()
        .filter_map(|d| {
            match d["relationType"].as_u64() {
                Some(1 | 6) => Some(Ok(DependencyRelation::Embedded)),
                Some(2) => Some(Ok(DependencyRelation::Optional)),
                Some(3) => Some(Ok(DependencyRelation::Required)),
                Some(5) => Some(Ok(DependencyRelation::Incompatible)),
                Some(4) => None,
                _ => Some(Err(OnlineError::InvalidResponse)),
            }
            .map(|r| {
                r.and_then(|r| {
                    Ok(Dependency::new(
                        Some(ProjectRef::new(Platform::CurseForge, id(d, "modId")?)),
                        None,
                        r,
                    ))
                })
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ContentVersion {
        reference,
        name: text(data, "displayName")?.into(),
        channel: match data["releaseType"].as_u64() {
            Some(1) => ReleaseChannel::Release,
            Some(2) => ReleaseChannel::Beta,
            Some(3) => ReleaseChannel::Alpha,
            _ => return Err(OnlineError::InvalidResponse),
        },
        published: timestamp(data, "fileDate"),
        game_versions: strings(data, "gameVersions"),
        loaders: strings(data, "gameVersions")
            .iter()
            .filter_map(|s| loader(s))
            .collect(),
        files: vec![file],
        dependencies,
    })
}
pub(super) async fn identify(
    api: ApiClient,
    files: Vec<FileIdentity>,
) -> Result<Vec<IdentifiedFile>, OnlineError> {
    let mut found = vec![];
    for chunk in files.chunks(100) {
        let data=api.request(reqwest::Method::POST,&["fingerprints","432"],&[],Some(json!({"fingerprints":chunk.iter().map(FileIdentity::fingerprint).collect::<Vec<_>>()}))).await?;
        found.extend(array(&data["data"], "exactMatches")?.clone());
    }
    let mut pages = std::collections::HashMap::new();
    for value in &found {
        let project = id(&value["file"], "modId")?;
        if let std::collections::hash_map::Entry::Vacant(entry) = pages.entry(project.clone()) {
            let page = project_downloads(&api, &project).await?;
            entry.insert(page);
        }
    }
    files
        .into_iter()
        .map(|identity| {
            let matches = found
                .iter()
                .filter(|v| {
                    v["file"]["fileFingerprint"].as_u64() == Some(identity.fingerprint() as u64)
                })
                // Fingerprints alone are weak: require the API's full-file SHA1 to agree as well.
                .filter(|v| {
                    v["file"]["hashes"].as_array().is_some_and(|hashes| {
                        hashes
                            .iter()
                            .any(|h| h["algo"] == 1 && h["value"].as_str() == Some(identity.sha1()))
                    })
                })
                .map(|v| {
                    let project = id(&v["file"], "modId")?;
                    let (page, allowed) =
                        pages.get(&project).ok_or(OnlineError::InvalidResponse)?;
                    parse_file(&v["file"], page, *allowed)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(IdentifiedFile { identity, matches })
        })
        .collect()
}
pub(super) async fn categories(
    api: ApiClient,
    kind: ContentKind,
) -> Result<Vec<ContentCategory>, OnlineError> {
    let data = metadata(&api).await?;
    let class = find_class(&data, kind)?;
    let mut descendants = vec![class];
    let all = array(&data, "data")?;
    loop {
        let before = descendants.len();
        for v in all {
            if v["parentCategoryId"]
                .as_u64()
                .is_some_and(|id| descendants.contains(&id.to_string()))
            {
                let child = id(v, "id")?;
                if !descendants.contains(&child) {
                    descendants.push(child);
                }
            }
        }
        if before == descendants.len() {
            break;
        }
    }
    all.iter()
        .filter(|v| {
            v["isClass"].as_bool() != Some(true)
                && v["id"]
                    .as_u64()
                    .is_some_and(|id| descendants.contains(&id.to_string()))
        })
        .map(|v| Ok(ContentCategory::new(id(v, "id")?, text(v, "name")?)))
        .collect()
}
