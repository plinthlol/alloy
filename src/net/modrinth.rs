// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// modrinth API v2 client: search, list versions, download files. keyless,
// unlike curseforge. like fabric.rs/quilt.rs, every public fn has a
// `_from(base)` variant so tests can point at a wiremock server.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::instance::models::ModLoader;
use crate::net::{HttpClient, NetError, download_file};
use std::path::Path;

const MODRINTH_API_BASE: &str = "https://api.modrinth.com/v2";

const SEARCH_CACHE_TTL: Duration = Duration::from_secs(60);
const VERSIONS_CACHE_TTL: Duration = Duration::from_secs(300);
const MAX_CACHE_ENTRIES: usize = 200;

static SEARCH_CACHE: LazyLock<Mutex<HashMap<String, (Instant, SearchResponse)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
type CachedVersions = Vec<ProjectVersion>;
static VERSIONS_CACHE: LazyLock<Mutex<HashMap<String, (Instant, CachedVersions)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_get<T: Clone>(
    map: &Mutex<HashMap<String, (Instant, T)>>,
    key: &str,
    ttl: Duration,
) -> Option<T> {
    map.lock()
        .ok()?
        .get(key)
        .filter(|(at, _)| at.elapsed() < ttl)
        .map(|(_, v)| v.clone())
}

fn cache_put<T>(map: &Mutex<HashMap<String, (Instant, T)>>, key: String, value: T) {
    if let Ok(mut guard) = map.lock() {
        if guard.len() >= MAX_CACHE_ENTRIES {
            guard.clear();
        }
        guard.insert(key, (Instant::now(), value));
    }
}

// modrinth loader facet strings. vanilla has no loader tag, so it maps to
// None and callers skip the filter.
fn loader_facet(loader: ModLoader) -> Option<&'static str> {
    match loader {
        ModLoader::Vanilla => None,
        ModLoader::Fabric => Some("fabric"),
        ModLoader::Forge => Some("forge"),
        ModLoader::NeoForge => Some("neoforge"),
        ModLoader::Quilt => Some("quilt"),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub total_hits: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchHit {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub project_type: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectVersion {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub version_number: String,
    pub game_versions: Vec<String>,
    pub loaders: Vec<String>,
    // "release" | "beta" | "alpha" — surfaced as a channel badge in the
    // version list so prereleases are visually distinct.
    #[serde(default)]
    pub version_type: String,
    pub files: Vec<VersionFile>,
    #[serde(default)]
    pub dependencies: Vec<VersionDependency>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionFile {
    pub url: String,
    pub filename: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub hashes: VersionHashes,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct VersionHashes {
    pub sha1: Option<String>,
    pub sha512: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionDependency {
    pub project_id: Option<String>,
    pub version_id: Option<String>,
    pub dependency_type: String,
}

// searches modrinth projects. `project_type` is "modpack" or "mod";
// game_version/loader narrow the results, offset/limit page through them.
#[allow(clippy::too_many_arguments)]
pub async fn search(
    client: &HttpClient,
    query: &str,
    project_type: &str,
    game_version: Option<&str>,
    loader: Option<ModLoader>,
    offset: u32,
    limit: u32,
) -> Result<SearchResponse, NetError> {
    search_from(
        client,
        MODRINTH_API_BASE,
        query,
        project_type,
        game_version,
        loader,
        offset,
        limit,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn search_from(
    client: &HttpClient,
    api_base: &str,
    query: &str,
    project_type: &str,
    game_version: Option<&str>,
    loader: Option<ModLoader>,
    offset: u32,
    limit: u32,
) -> Result<SearchResponse, NetError> {
    let mut facet_groups: Vec<String> = vec![format!("[\"project_type:{}\"]", project_type)];
    if let Some(gv) = game_version {
        facet_groups.push(format!("[\"versions:{}\"]", gv));
    }
    if let Some(l) = loader.and_then(loader_facet) {
        facet_groups.push(format!("[\"categories:{}\"]", l));
    }
    let facets = format!("[{}]", facet_groups.join(","));

    // an empty query (plain browse) has nothing to score relevance against,
    // so sort by downloads instead — a top-mods listing beats arbitrary order.
    let index = if query.trim().is_empty() { "downloads" } else { "relevance" };

    let url = format!(
        "{}/search?query={}&facets={}&offset={}&limit={}&index={}",
        api_base,
        urlencode(query),
        urlencode(&facets),
        offset,
        limit,
        index,
    );
    if let Some(cached) = cache_get(&SEARCH_CACHE, &url, SEARCH_CACHE_TTL) {
        return Ok(cached);
    }
    tracing::debug!("Searching Modrinth: {}", url);
    let resp: SearchResponse = client.get_json(&url).await?;
    tracing::debug!(
        "Modrinth search returned {} hit(s) of {} total",
        resp.hits.len(),
        resp.total_hits
    );
    cache_put(&SEARCH_CACHE, url, resp.clone());
    Ok(resp)
}

// lists a project's versions, optionally filtered by game version/loader.
// the endpoint takes the same filters as query params, so we reuse them.
pub async fn get_project_versions(
    client: &HttpClient,
    project_id: &str,
    game_version: Option<&str>,
    loader: Option<ModLoader>,
) -> Result<Vec<ProjectVersion>, NetError> {
    get_project_versions_from(client, MODRINTH_API_BASE, project_id, game_version, loader).await
}

pub async fn get_project_versions_from(
    client: &HttpClient,
    api_base: &str,
    project_id: &str,
    game_version: Option<&str>,
    loader: Option<ModLoader>,
) -> Result<Vec<ProjectVersion>, NetError> {
    let mut url = format!("{}/project/{}/version?", api_base, project_id);
    if let Some(gv) = game_version {
        url.push_str(&format!("game_versions=[\"{}\"]&", urlencode(gv)));
    }
    if let Some(l) = loader.and_then(loader_facet) {
        url.push_str(&format!("loaders=[\"{}\"]&", l));
    }
    if let Some(cached) = cache_get(&VERSIONS_CACHE, &url, VERSIONS_CACHE_TTL) {
        return Ok(cached);
    }
    tracing::debug!("Fetching Modrinth versions for project {}", project_id);
    let versions: Vec<ProjectVersion> = client.get_json(&url).await?;
    tracing::debug!(
        "Fetched {} Modrinth version(s) for project {}",
        versions.len(),
        project_id
    );
    cache_put(&VERSIONS_CACHE, url, versions.clone());
    Ok(versions)
}

pub async fn get_version(client: &HttpClient, version_id: &str) -> Result<ProjectVersion, NetError> {
    get_version_from(client, MODRINTH_API_BASE, version_id).await
}

// the full project page: `body` is the long-form markdown description
// rendered by tui/widgets/markdown.rs (description popups in the browse
// UIs). `description` is the short search-summary and is carried along for
// callers that want it, but the body is the point.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectBody {
    pub slug: String,
    pub title: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub description: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub body: String,
    // project screenshots. the standalone /gallery routes are write-only
    // (auth'd uploads) — reads come embedded here.
    #[serde(default)]
    pub gallery: Vec<GalleryImage>,
    // fields from the Modrinth API that we don't use but need to deserialize
    #[serde(default)]
    pub client_side: Option<String>,
    #[serde(default)]
    pub server_side: Option<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub downloads: Option<u64>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub project_type: Option<String>,
    #[serde(default)]
    pub licenses: Vec<String>,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default)]
    pub featured_versions: Vec<String>,
    #[serde(default)]
    pub color: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GalleryImage {
    pub url: String,
    // full-resolution counterpart to `url` (a ~350px webp thumbnail)
    #[serde(default)]
    pub raw_url: Option<String>,
    #[serde(default, deserialize_with = "null_to_default")]
    pub title: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub description: String,
    #[serde(default)]
    pub featured: bool,
    #[serde(default)]
    pub ordering: i64,
}

// #[serde(default)] ignores explicit nulls; Modrinth sends some.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    let value: Option<T> = Option::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}

pub async fn get_project(
    client: &HttpClient,
    project_id: &str,
) -> Result<ProjectBody, NetError> {
    get_project_from(client, MODRINTH_API_BASE, project_id).await
}

pub async fn get_project_from(
    client: &HttpClient,
    api_base: &str,
    project_id: &str,
) -> Result<ProjectBody, NetError> {
    let url = format!("{api_base}/project/{project_id}");
    tracing::debug!("Fetching Modrinth project {}", project_id);
    client.get_json(&url).await
}

pub async fn get_version_from(
    client: &HttpClient,
    api_base: &str,
    version_id: &str,
) -> Result<ProjectVersion, NetError> {
    let url = format!("{}/version/{}", api_base, version_id);
    tracing::debug!("Fetching Modrinth version {}", version_id);
    client.get_json(&url).await
}

// maps file hashes (sha1, lowercase hex) to the Modrinth version that
// provides them, via the batch `POST /v2/version_files` endpoint. used by
// modpack installs: mrpack entries carry no project id, only download
// urls, and hashing each downloaded file is the reliable way to recover
// the (project, version) identity the installed-content sidecar tracks.
// hashes absent from Modrinth are simply missing from the returned map.
pub async fn lookup_versions_by_sha1(
    client: &HttpClient,
    sha1s: &[String],
) -> Result<HashMap<String, ProjectVersion>, NetError> {
    lookup_versions_by_sha1_from(client, MODRINTH_API_BASE, sha1s).await
}

pub async fn lookup_versions_by_sha1_from(
    client: &HttpClient,
    api_base: &str,
    sha1s: &[String],
) -> Result<HashMap<String, ProjectVersion>, NetError> {
    if sha1s.is_empty() {
        return Ok(HashMap::new());
    }
    // request shape per API docs: {"hashes": [...], "algorithm": "sha1"}
    // ("fingerprints" is for murmur/sha1-of-sha1 lookups instead).
    let url = format!("{api_base}/version_files");
    let body = serde_json::json!({ "hashes": sha1s, "algorithm": "sha1" });
    let map: HashMap<String, ProjectVersion> = client.post_json(&url, &body).await?;
    Ok(map)
}

// downloads a version's primary file (first file as fallback — modrinth
// always flags one, but don't trust that blindly) to `dest`. used for plain
// mod jars and `.mrpack` files alike.
pub async fn download_primary_file(
    client: &HttpClient,
    version: &ProjectVersion,
    dest: &Path,
    progress_cb: impl Fn(u64, u64),
) -> Result<(), NetError> {
    let file = version
        .files
        .iter()
        .find(|f| f.primary)
        .or_else(|| version.files.first())
        .ok_or_else(|| NetError::Parse(format!("Version {} has no files", version.id)))?;

    tracing::info!("Downloading Modrinth file {} for {}", file.filename, version.id);
    download_file(client, &file.url, dest, progress_cb).await
}

// minimal percent-encoding: our query params only hold ascii we produce
// plus user search text, so a full RFC 3986 impl would be overkill.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_leaves_safe_chars_alone() {
        assert_eq!(urlencode("fabric-1.20.1"), "fabric-1.20.1");
    }

    // regression: Modrinth sends "description": null in gallery items,
    // which used to kill the whole /v2/project parse
    #[test]
    fn project_body_tolerates_explicit_nulls() {
        let json = serde_json::json!({
            "slug": "example",
            "title": "Example",
            "gallery": [{
                "url": "https://cdn.modrinth.com/img.png",
                "title": "Logo",
                "description": null,
                "featured": false,
                "ordering": 0
            }]
        });
        let parsed: ProjectBody = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.slug, "example");
        assert_eq!(parsed.gallery[0].title, "Logo");
        assert_eq!(parsed.gallery[0].description, "");

        // and a fully-null text field lands on the default too
        let json: serde_json::Value = serde_json::json!({
            "slug": "example",
            "title": "Example",
            "description": null,
            "body": null
        });
        let parsed: ProjectBody = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.description, "");
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn urlencode_escapes_special_chars() {
        assert_eq!(urlencode("[\"a\"]"), "%5B%22a%22%5D");
        assert_eq!(urlencode("hello world"), "hello%20world");
    }

    #[test]
    fn loader_facet_maps_known_loaders() {
        assert_eq!(loader_facet(ModLoader::Fabric), Some("fabric"));
        assert_eq!(loader_facet(ModLoader::Vanilla), None);
    }

    #[test]
    fn cache_roundtrip_and_ttl_expiry() {
        let map: Mutex<HashMap<String, (Instant, u32)>> = Mutex::new(HashMap::new());
        cache_put(&map, "k".into(), 42u32);
        assert_eq!(cache_get(&map, "k", Duration::from_secs(60)), Some(42));
        assert_eq!(cache_get(&map, "k", Duration::ZERO), None);
    }

    #[test]
    fn cache_missing_key_is_none() {
        let map: Mutex<HashMap<String, (Instant, u32)>> = Mutex::new(HashMap::new());
        assert_eq!(cache_get(&map, "nope", Duration::from_secs(60)), None);
    }

    #[test]
    fn cache_put_clears_when_over_capacity() {
        let map: Mutex<HashMap<String, (Instant, u32)>> = Mutex::new(HashMap::new());
        for i in 0..MAX_CACHE_ENTRIES {
            cache_put(&map, format!("k{i}"), i as u32);
        }
        assert_eq!(map.lock().unwrap().len(), MAX_CACHE_ENTRIES);
        cache_put(&map, "fresh".into(), 1);
        let guard = map.lock().unwrap();
        assert_eq!(guard.len(), 1);
        assert!(guard.contains_key("fresh"));
    }

    #[tokio::test]
    async fn search_is_cached_per_query() {
        let server = wiremock::MockServer::start().await;
        let body = || serde_json::json!({ "hits": [], "total_hits": 0 });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::query_param("query", "sodium"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body()))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::query_param("query", "sodium2"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body()))
            .expect(1)
            .mount(&server)
            .await;
        let client = HttpClient::new();
        let base = server.uri();

        let first = search_from(&client, &base, "sodium", "mod", Some("1.20.1"), Some(ModLoader::Fabric), 0, 40)
            .await
            .unwrap();
        let second = search_from(&client, &base, "sodium", "mod", Some("1.20.1"), Some(ModLoader::Fabric), 0, 40)
            .await
            .unwrap();
        assert_eq!(first.total_hits, second.total_hits);

        search_from(&client, &base, "sodium2", "mod", Some("1.20.1"), Some(ModLoader::Fabric), 0, 40)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn versions_are_cached_per_project() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/project/abc/version"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([])),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = HttpClient::new();
        let base = server.uri();

        let first = get_project_versions_from(&client, &base, "abc", Some("1.20.1"), Some(ModLoader::Fabric))
            .await
            .unwrap();
        let second = get_project_versions_from(&client, &base, "abc", Some("1.20.1"), Some(ModLoader::Fabric))
            .await
            .unwrap();
        assert_eq!(first.len(), second.len());
    }

    #[tokio::test]
    #[ignore = "hits live Modrinth API"]
    async fn test_search_modpacks() {
        let client = HttpClient::new();
        let resp = search(&client, "", "modpack", Some("1.20.1"), Some(ModLoader::Fabric), 0, 10)
            .await
            .unwrap();
        assert!(!resp.hits.is_empty());
    }
}
