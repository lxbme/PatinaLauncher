//! Official stable Linux x86_64 JDK release lists. Microsoft publishes only
//! current builds in its maintained Markdown download tables, not a history API.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JavaVendor {
    Temurin,
    Zulu,
    Microsoft,
}
impl JavaVendor {
    pub const ALL: [Self; 3] = [Self::Temurin, Self::Zulu, Self::Microsoft];
    pub fn name(self) -> &'static str {
        match self {
            Self::Temurin => "Eclipse Temurin",
            Self::Zulu => "Azul Zulu",
            Self::Microsoft => "Microsoft OpenJDK",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Self::Temurin => "temurin",
            Self::Zulu => "zulu",
            Self::Microsoft => "microsoft",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRelease {
    vendor: JavaVendor,
    major: u32,
    version: String,
    build: Vec<u32>,
    url: String,
    size: Option<u64>,
    sha256: Option<String>,
    checksum_url: Option<String>,
}
impl JavaRelease {
    pub fn vendor(&self) -> JavaVendor {
        self.vendor
    }
    pub fn major(&self) -> u32 {
        self.major
    }
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn size(&self) -> Option<u64> {
        self.size
    }
    pub fn sha256(&self) -> Option<&str> {
        self.sha256.as_deref()
    }
    pub fn checksum_url(&self) -> Option<&str> {
        self.checksum_url.as_deref()
    }
    pub fn id(&self) -> String {
        format!(
            "{}-{}",
            self.vendor.id(),
            self.version.replace(['+', '.', '_'], "-")
        )
    }
    pub fn is_newer_than(&self, old: &Self) -> bool {
        self.vendor == old.vendor && self.major == old.major && self.build > old.build
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("{}", rust_i18n::t!("java_distribution.catalog", error = .0))]
    Unavailable(String),
}
fn bad(reason: impl std::fmt::Display) -> CatalogError {
    CatalogError::Unavailable(reason.to_string())
}
fn json(s: &str) -> Result<Value, CatalogError> {
    serde_json::from_str(s).map_err(bad)
}
fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn numbers(s: &str) -> Vec<u32> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter_map(|n| n.parse().ok())
        .collect()
}
fn hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn parse_temurin(body: &str, major: u32) -> Result<Vec<JavaRelease>, CatalogError> {
    let value = json(body)?;
    let rows = value
        .as_array()
        .ok_or_else(|| bad(rust_i18n::t!("java_distribution.reason.release_array")))?;
    let mut out = Vec::new();
    for row in rows {
        if text(row, "release_type") != "ga"
            || row["version_data"]["major"].as_u64() != Some(major as u64)
        {
            continue;
        }
        let version = text(row, "release_name").trim_start_matches("jdk-");
        if version.is_empty() || version.contains("ea") {
            continue;
        }
        for bin in row["binaries"].as_array().into_iter().flatten() {
            if text(bin, "architecture") != "x64"
                || text(bin, "os") != "linux"
                || text(bin, "image_type") != "jdk"
                || text(bin, "jvm_impl") != "hotspot"
                || text(bin, "heap_size") != "normal"
            {
                continue;
            }
            let p = &bin["package"];
            if !text(p, "link").ends_with(".tar.gz") || !hash(text(p, "checksum")) {
                return Err(bad(rust_i18n::t!(
                    "java_distribution.reason.missing_archive"
                )));
            }
            out.push(JavaRelease {
                vendor: JavaVendor::Temurin,
                major,
                version: version.into(),
                build: ["major", "minor", "security", "build"]
                    .into_iter()
                    .map(|key| row["version_data"][key].as_u64().unwrap_or(0) as u32)
                    .collect(),
                url: text(p, "link").into(),
                size: p["size"].as_u64(),
                sha256: Some(text(p, "checksum").into()),
                checksum_url: None,
            });
        }
    }
    Ok(out)
}

pub fn parse_zulu(body: &str, major: u32) -> Result<Option<JavaRelease>, CatalogError> {
    let row = json(body)?;
    if text(&row, "os") != "linux"
        || text(&row, "arch") != "x86"
        || row["hw_bitness"] != 64
        || text(&row, "archive_type") != "tar.gz"
        || text(&row, "java_package_type") != "jdk"
        || text(&row, "release_status") != "ga"
        || text(&row, "availability_type") != "CA"
        || text(&row, "lib_c_type") != "glibc"
        || row["javafx_bundled"] != false
        || row["crac_supported"] != false
    {
        return Ok(None);
    }
    let build: Vec<u32> = row["java_version"]
        .as_array()
        .ok_or_else(|| {
            bad(rust_i18n::t!(
                "java_distribution.reason.missing_java_version"
            ))
        })?
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as u32))
        .collect();
    if build.first() != Some(&major) {
        return Ok(None);
    }
    if !hash(text(&row, "sha256_hash")) || !text(&row, "download_url").ends_with(".tar.gz") {
        return Err(bad(rust_i18n::t!(
            "java_distribution.reason.missing_archive"
        )));
    }
    let java_version = build
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".");
    let distro = row["distro_version"]
        .as_array()
        .ok_or_else(|| bad(rust_i18n::t!("java_distribution.reason.missing_zulu_build")))?
        .iter()
        .filter_map(|v| v.as_u64())
        .collect::<Vec<_>>();
    let openjdk_build = row["openjdk_build_number"].as_u64().ok_or_else(|| {
        bad(rust_i18n::t!(
            "java_distribution.reason.missing_openjdk_build"
        ))
    })?;
    let version = format!(
        "{java_version}+{openjdk_build}-zulu{}",
        distro
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(".")
    );
    let mut order = build;
    order.extend(
        row["distro_version"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_u64().map(|n| n as u32)),
    );
    order.push(openjdk_build as u32);
    Ok(Some(JavaRelease {
        vendor: JavaVendor::Zulu,
        major,
        version,
        build: order,
        url: text(&row, "download_url").into(),
        size: row["size"].as_u64(),
        sha256: Some(text(&row, "sha256_hash").into()),
        checksum_url: None,
    }))
}

pub fn parse_microsoft(body: &str) -> Result<Vec<JavaRelease>, CatalogError> {
    // Match only an exact archive and its matching checksum on the same table row.
    let re = regex::Regex::new(
        r"https://aka\.ms/download-jdk/microsoft-jdk-([0-9]+(?:\.[0-9]+)+)-linux-x64\.tar\.gz",
    )
    .expect("static regex");
    let mut out = Vec::new();
    let mut stable = false;
    for line in body.lines() {
        if line.starts_with("## ") {
            stable = false;
        }
        if line.starts_with("### OpenJDK ") {
            stable = line.contains(" LTS |");
        }
        if !stable || !line.trim_start().starts_with('|') {
            continue;
        }
        let cells = line.split('|').map(str::trim).collect::<Vec<_>>();
        if cells.get(1) != Some(&"Linux")
            || cells.get(2) != Some(&"x64")
            || cells.get(3) != Some(&"`tar.gz`")
        {
            continue;
        }
        let c = re
            .captures(line)
            .ok_or_else(|| bad(rust_i18n::t!("java_distribution.reason.microsoft_row")))?;
        let url = c.get(0).unwrap().as_str();
        // A checksum-only row is not an archive row.
        if !line.contains(&format!("({url})")) {
            return Err(bad(rust_i18n::t!("java_distribution.reason.microsoft_row")));
        }
        let checksum = format!("{url}.sha256sum.txt");
        if !line.contains(&format!("({checksum})")) {
            return Err(bad(rust_i18n::t!(
                "java_distribution.reason.microsoft_checksum"
            )));
        }
        let version = c[1].to_string();
        let build = numbers(&version);
        let major = *build
            .first()
            .ok_or_else(|| bad(rust_i18n::t!("java_distribution.reason.missing_version")))?;
        if !out.iter().any(|r: &JavaRelease| r.url == url) {
            out.push(JavaRelease {
                vendor: JavaVendor::Microsoft,
                major,
                version,
                build,
                url: url.into(),
                size: None,
                sha256: None,
                checksum_url: Some(checksum),
            });
        }
    }
    if out.is_empty() {
        return Err(bad(rust_i18n::t!(
            "java_distribution.reason.microsoft_empty"
        )));
    }
    Ok(out)
}

#[derive(Clone)]
pub struct JavaCatalog {
    client: reqwest::Client,
    temurin: String,
    zulu: String,
    microsoft: String,
}
impl JavaCatalog {
    pub fn new(client: reqwest::Client) -> Self {
        Self::with_endpoints(
            client,
            "https://api.adoptium.net/v3",
            "https://api.azul.com/metadata/v1/zulu/packages/",
            "https://learn.microsoft.com/en-us/java/openjdk/download?accept=text/markdown",
        )
    }
    pub fn with_endpoints(
        client: reqwest::Client,
        temurin: &str,
        zulu: &str,
        microsoft: &str,
    ) -> Self {
        Self {
            client,
            temurin: temurin.trim_end_matches('/').into(),
            zulu: zulu.into(),
            microsoft: microsoft.into(),
        }
    }
    async fn get(&self, url: &str, query: &[(&str, String)]) -> Result<String, CatalogError> {
        let mut url = reqwest::Url::parse(url).map_err(bad)?;
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(k, v)| (*k, v)));
        let response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(45))
            .send()
            .await
            .map_err(bad)?;
        if response.status() == 404 && response.url().path().contains("/assets/feature_releases/") {
            return Ok("[]".into());
        }
        response
            .error_for_status()
            .map_err(bad)?
            .text()
            .await
            .map_err(bad)
    }
    async fn zulu_rows(&self, major: Option<u32>) -> Result<Vec<Value>, CatalogError> {
        let mut out = Vec::new();
        for page in 1..=100 {
            let mut q = vec![
                ("os", "linux".into()),
                ("arch", "x86".into()),
                ("hw_bitness", "64".into()),
                ("archive_type", "tar.gz".into()),
                ("java_package_type", "jdk".into()),
                ("release_status", "ga".into()),
                ("availability_types", "CA".into()),
                ("javafx_bundled", "false".into()),
                ("crac_supported", "false".into()),
                ("latest", "true".into()),
                ("page", page.to_string()),
                ("page_size", "100".into()),
            ];
            if let Some(m) = major {
                q.push(("java_version", m.to_string()));
            }
            let v = json(&self.get(&self.zulu, &q).await?)?;
            let rows = v
                .as_array()
                .ok_or_else(|| bad(rust_i18n::t!("java_distribution.reason.package_array")))?;
            out.extend(rows.iter().cloned());
            if rows.len() < 100 {
                return Ok(out);
            }
        }
        Err(bad(rust_i18n::t!(
            "java_distribution.reason.zulu_pagination"
        )))
    }
    pub async fn majors(&self, vendor: JavaVendor) -> Result<Vec<u32>, CatalogError> {
        let mut majors = match vendor {
            JavaVendor::Temurin => {
                let v = json(
                    &self
                        .get(&format!("{}/info/available_releases", self.temurin), &[])
                        .await?,
                )?;
                v["available_releases"]
                    .as_array()
                    .ok_or_else(|| bad(rust_i18n::t!("java_distribution.reason.missing_releases")))?
                    .iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u32))
                    .collect::<Vec<_>>()
            }
            JavaVendor::Zulu => self
                .zulu_rows(None)
                .await?
                .iter()
                .filter_map(|r| r["java_version"][0].as_u64().map(|n| n as u32))
                .collect(),
            JavaVendor::Microsoft => parse_microsoft(&self.get(&self.microsoft, &[]).await?)?
                .iter()
                .map(|r| r.major)
                .collect(),
        };
        majors.sort_unstable_by(|a, b| b.cmp(a));
        majors.dedup();
        Ok(majors)
    }
    pub async fn releases(
        &self,
        vendor: JavaVendor,
        major: u32,
    ) -> Result<Vec<JavaRelease>, CatalogError> {
        let mut releases = match vendor {
            JavaVendor::Temurin => {
                let mut out = Vec::new();
                let mut ended = false;
                for page in 0..100 {
                    let body = self
                        .get(
                            &format!("{}/assets/feature_releases/{major}/ga", self.temurin),
                            &[
                                ("architecture", "x64".into()),
                                ("os", "linux".into()),
                                ("image_type", "jdk".into()),
                                ("jvm_impl", "hotspot".into()),
                                ("heap_size", "normal".into()),
                                ("vendor", "eclipse".into()),
                                ("page_size", "100".into()),
                                ("page", page.to_string()),
                            ],
                        )
                        .await?;
                    if json(&body)?.as_array().is_some_and(Vec::is_empty) {
                        ended = true;
                        break;
                    }
                    let count = json(&body)?.as_array().map_or(0, Vec::len);
                    out.extend(parse_temurin(&body, major)?);
                    if count < 100 {
                        ended = true;
                        break;
                    }
                }
                if !ended {
                    return Err(bad(rust_i18n::t!(
                        "java_distribution.reason.temurin_pagination"
                    )));
                }
                out
            }
            JavaVendor::Zulu => {
                let mut out = Vec::new();
                for row in self.zulu_rows(Some(major)).await? {
                    let id = text(&row, "package_uuid");
                    if id.is_empty() {
                        return Err(bad(rust_i18n::t!(
                            "java_distribution.reason.missing_package_id"
                        )));
                    }
                    let body = self
                        .get(&format!("{}/{}", self.zulu.trim_end_matches('/'), id), &[])
                        .await?;
                    if let Some(r) = parse_zulu(&body, major)? {
                        out.push(r);
                    }
                }
                out
            }
            JavaVendor::Microsoft => parse_microsoft(&self.get(&self.microsoft, &[]).await?)?
                .into_iter()
                .filter(|r| r.major == major)
                .collect(),
        };
        releases.sort_by(|a, b| b.build.cmp(&a.build));
        releases.dedup_by(|a, b| a.id() == b.id());
        Ok(releases)
    }
}
