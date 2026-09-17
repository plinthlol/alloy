// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// networking layer: http client, downloads, and helpers for mojang/mod
// loader assets.

pub mod fabric;
pub mod forge;
pub mod curseforge;
pub mod java_provision;
pub mod mojang;
pub mod modrinth;
pub mod neoforge;
pub mod quilt;

use java_provision::ImageType;

use reqwest::Client;
use serde::de::DeserializeOwned;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use sha1::{Digest, Sha1};
use thiserror::Error;

// API hosts that get rate limited. downloads/CDNs stay ungated.
const MODRINTH_API_HOST: &str = "api.modrinth.com";
const CURSEFORGE_API_HOST: &str = "api.curseforge.com";

// max requests per host per window; excess callers queue.
const RATE_LIMIT_MAX_PER_WINDOW: usize = 2;
const RATE_LIMIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Error)]
pub enum NetError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Server returned error status {status}: {url}")]
    StatusError {
        status: u16,
        url: String,
        // numeric Retry-After from the server, if any
        retry_after: Option<std::time::Duration>,
    },
    #[error("invalid JSON from {url} (status {status}, body: {snippet})")]
    BadJson {
        url: String,
        status: u16,
        snippet: String,
    },
    #[error("Task failed: {0}")]
    TaskFailed(String),
}

#[derive(Clone)]
pub struct HttpClient {
    inner: Client,
    // per-host rate limiters, shared across clones
    limiters: Arc<Mutex<HashMap<&'static str, Arc<RateLimiter>>>>,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

// hard cap for web-fetched assets (inline project images). guards against
// a hostile/huge URL burning memory in the markdown description view.
pub const MAX_PROVIDER_ASSET_BYTES: usize = 16 * 1024 * 1024;

impl HttpClient {
    pub fn new() -> Self {
        let user_agent = format!("alloy/{} (Minecraft Launcher)", env!("CARGO_PKG_VERSION"));
        let client = Client::builder()
            .user_agent(user_agent.clone())
            .timeout(std::time::Duration::from_secs(30))
            // send small requests (API JSON, headers) out immediately
            // instead of letting Nagle's algorithm hold them for a
            // coalescing window — a classic 40-200ms stall per call.
            .tcp_nodelay(true)
            // hold pooled keep-alive connections open longer than the 90s
            // default, so a browse session's warmed connections survive
            // pauses between searches/installs.
            .pool_idle_timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "Failed to build configured HTTP client, falling back to reqwest default: {}",
                    e
                );
                Client::new()
            });
        tracing::trace!("Created HTTP client with user-agent '{}'", user_agent);
        Self {
            inner: client,
            limiters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // process-wide shared client for the hot UI paths (catalog searches,
    // version listings, single-file installs). reqwest pools connections
    // per-client, so a fresh client per fetch re-pays TCP+TLS on every
    // call — sharing one means the search warms a keep-alive connection
    // that the version fetch (and the next search) reuses for free. cheap
    // to clone (the pool is internally Arc'd), so callers keep local
    // bindings like with new().
    pub fn shared() -> Self {
        static SHARED: LazyLock<HttpClient> = LazyLock::new(HttpClient::new);
        SHARED.clone()
    }

    pub async fn get(&self, url: &str) -> Result<reqwest::Response, NetError> {
        self.gate(url).await;
        self.gate(url).await;
        tracing::trace!("HTTP GET {}", url);
        let response = self.inner.get(url).send().await?;
        if !response.status().is_success() {
            let retry_after = parse_retry_after(&response);
            tracing::debug!(
                "HTTP GET {} returned non-success status {}",
                url,
                response.status()
            );
            return Err(NetError::StatusError {
                status: response.status().as_u16(),
                url: url.to_string(),
                retry_after,
            });
        }
        tracing::trace!("HTTP GET {} succeeded with {}", url, response.status());
        Ok(response)
    }

    // like `get`, but with extra headers — e.g. CurseForge's `x-api-key`.
    // `headers` is (name, value) pairs.
    pub async fn get_with_headers(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<reqwest::Response, NetError> {
        self.gate(url).await;
        tracing::trace!("HTTP GET {} ({} extra header(s))", url, headers.len());
        let mut req = self.inner.get(url);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let response = req.send().await?;
        if !response.status().is_success() {
            let retry_after = parse_retry_after(&response);
            tracing::debug!(
                "HTTP GET {} returned non-success status {}",
                url,
                response.status()
            );
            return Err(NetError::StatusError {
                status: response.status().as_u16(),
                url: url.to_string(),
                retry_after,
            });
        }
        tracing::trace!("HTTP GET {} succeeded with {}", url, response.status());
        Ok(response)
    }

    // like get_bytes, but rejects bodies over `limit` bytes — both via the
    // declared content-length up front and while streaming chunks, so a
    // server that lies about the length still can't blow the cap.
    pub async fn get_bytes_limited(&self, url: &str, limit: usize) -> Result<Vec<u8>, NetError> {
        get_with_retry(self, url, move |mut response| async move {
            if response
                .content_length()
                .is_some_and(|length| length > limit as u64)
            {
                return Err(NetError::Parse(format!(
                    "Response exceeds the {limit}-byte limit"
                )));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if bytes.len().saturating_add(chunk.len()) > limit {
                    return Err(NetError::Parse(format!(
                        "Response exceeds the {limit}-byte limit"
                    )));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        })
        .await
    }

    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, NetError> {
        let url_owned = url.to_string();
        get_with_retry(self, url, move |resp| {
            let url = url_owned.clone();
            async move { decode_json_response(resp, url).await }
        })
        .await
    }

    // POST a JSON body and deserialize the JSON response, with the same
    // retry/backoff envelope as get_json. used by hash-lookup endpoints
    // (Modrinth's POST /v2/version_files) where the query is too large
    // for a GET.
    pub async fn post_json<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, NetError> {
        let url_owned = url.to_string();
        // retry envelope mirrors get_with_retry, but through POST
        for attempt in 0..=MAX_RETRIES {
            match self.inner.post(url).json(body).send().await {
                Ok(resp) => {
                    let resp = ensure_success(resp)?;
                    match decode_json_response(resp, url_owned.clone()).await {
                        Ok(value) => return Ok(value),
                        Err(e) if is_retryable(&e) && attempt < MAX_RETRIES => {
                            sleep_before_retry("request", url, attempt, &e).await;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => {
                    let err = NetError::from(e);
                    if is_retryable(&err) && attempt < MAX_RETRIES {
                        sleep_before_retry("request", url, attempt, &err).await;
                    } else {
                        return Err(err);
                    }
                }
            }
        }
        unreachable!("retry loop returns on success or final error")
    }

    // header-carrying counterpart to `get_json`, retried the same way.
    pub async fn get_json_with_headers<T: DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<T, NetError> {
        let url_owned = url.to_string();
        get_with_retry_headers(self, url, headers, move |resp| {
            let url = url_owned.clone();
            async move { decode_json_response(resp, url).await }
        })
        .await
    }

    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, NetError> {
        get_with_retry(
            self,
            url,
            |resp| async move { Ok(resp.bytes().await?.to_vec()) },
        )
        .await
    }

    // fetch JSON but keep the raw bytes too: install paths want the parsed
    // shape *and* the exact bytes, to write the loader-profiles cache
    // byte-for-byte so unknown fields survive.
    pub async fn get_json_with_raw<T: DeserializeOwned>(
        &self,
        url: &str,
        label: &str,
    ) -> Result<(T, Vec<u8>), NetError> {
        tracing::debug!("Fetching {} JSON from {}", label, url);
        let raw = self.get_bytes(url).await?;
        tracing::trace!("Fetched {} byte(s) for {}", raw.len(), label);
        let parsed: T = serde_json::from_slice(&raw)
            .map_err(|e| NetError::Parse(format!("Failed to parse {label}: {e}")))?;
        Ok((parsed, raw))
    }
}

// numeric Retry-After only; HTTP-dates fall back to backoff.
fn parse_retry_after(response: &reqwest::Response) -> Option<std::time::Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let secs: u64 = value.to_str().ok()?.trim().parse().ok()?;
    Some(std::time::Duration::from_secs(secs))
}

// maps a url to its rate-limited host, None = ungated.
fn rate_limited_host(url: &str) -> Option<&'static str> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    // drop userinfo (user@host) if present, then the path/query
    let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // strip a trailing :port so host:port spellings still match the bare
    // host constants
    let host = authority.split_once(':').map_or(authority, |(h, _)| h);
    match host {
        MODRINTH_API_HOST => Some(MODRINTH_API_HOST),
        CURSEFORGE_API_HOST => Some(CURSEFORGE_API_HOST),
        _ => None,
    }
}

impl HttpClient {
    // wait for a slot before sending; no-op for non-throttled hosts.
    async fn gate(&self, url: &str) {
        let Some(host) = rate_limited_host(url) else {
            return;
        };
        let limiter = {
            let mut map = self.limiters.lock().expect("rate limiter map poisoned");
            Arc::clone(map.entry(host).or_insert_with(|| Arc::new(RateLimiter::new())))
        };
        limiter.acquire().await;
    }
}

// rolling window: timestamps of recent sends; full window = wait for
// the oldest one to age out.
struct RateLimiter {
    recent: tokio::sync::Mutex<VecDeque<tokio::time::Instant>>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            recent: tokio::sync::Mutex::new(VecDeque::new()),
        }
    }

    // reserves a slot, waiting until the window has room.
    async fn acquire(&self) {
        loop {
            let now = tokio::time::Instant::now();
            let wait_until = {
                let mut recent = self.recent.lock().await;
                // drop timestamps that have slid out of the window
                while let Some(&front) = recent.front() {
                    if now.duration_since(front) >= RATE_LIMIT_WINDOW {
                        recent.pop_front();
                    } else {
                        break;
                    }
                }
                if recent.len() < RATE_LIMIT_MAX_PER_WINDOW {
                    recent.push_back(now);
                    None
                } else {
                    // window is full: the earliest slot frees up one window
                    // after it was taken
                    Some(recent.front().copied().expect("non-empty above") + RATE_LIMIT_WINDOW)
                }
            };
            match wait_until {
                None => return,
                Some(at) => tokio::time::sleep_until(at).await,
            }
        }
    }
}

// shared retry envelope: retries transient failures (timeouts, connect
// errors, 5xx) with exponential backoff. used by get_json and get_bytes.
async fn get_with_retry<T, F, Fut>(client: &HttpClient, url: &str, decode: F) -> Result<T, NetError>
where
    F: Fn(reqwest::Response) -> Fut,
    Fut: std::future::Future<Output = Result<T, NetError>>,
{
    for attempt in 0..=MAX_RETRIES {
        match client.get(url).await {
            Ok(resp) => match decode(resp).await {
                Ok(value) => return Ok(value),
                Err(e) if is_retryable(&e) => {
                    if attempt == MAX_RETRIES {
                        return Err(e);
                    }
                    sleep_before_retry("request", url, attempt, &e).await;
                }
                Err(e) => return Err(e),
            },
            Err(e) if is_retryable(&e) => {
                if attempt == MAX_RETRIES {
                    return Err(e);
                }
                sleep_before_retry("request", url, attempt, &e).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("retry loop returns on success or final error")
}

const MAX_RETRIES: u32 = 3;
const RETRY_BASE_DELAY_MS: u64 = 667;
// upper bound on a server-suggested Retry-After we'll actually wait
const RETRY_AFTER_CAP: std::time::Duration = std::time::Duration::from_secs(60);

// same envelope, but with extra headers (CurseForge's `x-api-key`).
// separate fn so the plain path doesn't take `&[]` at every call site.
async fn get_with_retry_headers<T, F, Fut>(
    client: &HttpClient,
    url: &str,
    headers: &[(&str, &str)],
    decode: F,
) -> Result<T, NetError>
where
    F: Fn(reqwest::Response) -> Fut,
    Fut: std::future::Future<Output = Result<T, NetError>>,
{
    for attempt in 0..=MAX_RETRIES {
        match client.get_with_headers(url, headers).await {
            Ok(resp) => match decode(resp).await {
                Ok(value) => return Ok(value),
                Err(e) if is_retryable(&e) => {
                    if attempt == MAX_RETRIES {
                        return Err(e);
                    }
                    sleep_before_retry("request", url, attempt, &e).await;
                }
                Err(e) => return Err(e),
            },
            Err(e) if is_retryable(&e) => {
                if attempt == MAX_RETRIES {
                    return Err(e);
                }
                sleep_before_retry("request", url, attempt, &e).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("retry loop returns on success or final error")
}

// turns a non-2xx response into a NetError::StatusError (capturing
// Retry-After for the backoff logic). POST callers share this with the
// GET paths.
fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response, NetError> {
    if !response.status().is_success() {
        let retry_after = parse_retry_after(&response);
        let url = response.url().to_string();
        tracing::debug!("HTTP request {} returned non-success status {}", url, response.status());
        return Err(NetError::StatusError {
            status: response.status().as_u16(),
            url,
            retry_after,
        });
    }
    Ok(response)
}

async fn decode_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
    url: String,
) -> Result<T, NetError> {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.bytes().await?;
    serde_json::from_slice(&bytes).map_err(|error| {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
            .escape_debug()
            .collect::<String>();
        // serde error included so parse failures are debuggable
        tracing::debug!(
            "JSON decode failed for {} (status {}, content-type {}, {} bytes): {} — {}",
            url,
            status,
            content_type,
            bytes.len(),
            error,
            head
        );
        NetError::BadJson {
            url,
            status,
            snippet: format!("{content_type}: {head} — {error}"),
        }
    })
}

async fn sleep_before_retry(kind: &str, url: &str, attempt: u32, err: &NetError) {
    // Retry-After wins when present (capped); else exponential backoff
    let server_delay = if let NetError::StatusError {
        retry_after: Some(d),
        ..
    } = err
    {
        Some((*d).min(RETRY_AFTER_CAP))
    } else {
        None
    };
    let delay =
        server_delay.unwrap_or_else(|| std::time::Duration::from_millis(RETRY_BASE_DELAY_MS * 2u64.pow(attempt)));
    tracing::warn!(
        "{} failed, retrying after {}ms ({}, attempt {}/{}): {}: {}",
        kind,
        delay.as_millis(),
        if server_delay.is_some() { "Retry-After" } else { "backoff" },
        attempt + 2,
        MAX_RETRIES + 1,
        url,
        err
    );
    tokio::time::sleep(delay).await;
}

// streams a file to disk, calling progress_cb(downloaded, total). total is
// 0 if the server sends no content-length, so callers should handle that.
// retries transient failures with backoff.
pub async fn download_file(
    client: &HttpClient,
    url: &str,
    dest: &Path,
    progress_cb: impl Fn(u64, u64),
) -> Result<(), NetError> {
    tracing::debug!("Downloading {} to {}", url, dest.display());

    for attempt in 0..=MAX_RETRIES {
        match download_file_once(client, url, dest, &progress_cb).await {
            Ok(()) => {
                tracing::debug!("Downloaded {} to {}", url, dest.display());
                return Ok(());
            }
            Err(e) if is_retryable(&e) => {
                if attempt == MAX_RETRIES {
                    return Err(e);
                }
                sleep_before_retry("download", url, attempt, &e).await;
            }
            Err(e) => return Err(e),
        }
    }

    unreachable!("retry loop returns on success or final error")
}

// one download attempt. writes to a `.part` file and renames into place
// only after the full body lands, so a mid-stream failure never leaves a
// truncated file at `dest` masquerading as a complete download.
async fn download_file_once(
    client: &HttpClient,
    url: &str,
    dest: &Path,
    progress_cb: &impl Fn(u64, u64),
) -> Result<(), NetError> {
    let response = client.get(url).await?;
    let total = response.content_length().unwrap_or(0);
    tracing::trace!("Download content length for {}: {}", url, total);

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let tmp_dest = tmp_download_path(dest);
    let result = write_stream_to_file(response, &tmp_dest, progress_cb).await;

    match result {
        Ok(()) => {
            tokio::fs::rename(&tmp_dest, dest).await?;
            Ok(())
        }
        Err(e) => {
            // best-effort cleanup; the temp file is harmless leftover clutter
            // at worst, so don't let a cleanup failure mask the real error.
            if let Err(cleanup_err) = tokio::fs::remove_file(&tmp_dest).await
                && cleanup_err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    "Failed to clean up partial download {}: {}",
                    tmp_dest.display(),
                    cleanup_err
                );
            }
            Err(e)
        }
    }
}

// verifies `path`'s contents against an expected sha1 hex digest. an empty
// expected digest skips verification (defensive — some manifests omit it).
// used by the mojang/modrinth download paths so a truncated or corrupted
// file never lands in the cache as good: matching is done on the full
// digest, compared case-insensitively since some sources pad/uppercase.
pub async fn verify_sha1(path: &Path, expected: &str) -> Result<(), NetError> {
    let bytes = tokio::fs::read(path).await?;
    verify_bytes_sha1(&bytes, expected)
}

// in-memory counterpart to verify_sha1, for payloads already held in RAM
// (e.g. the asset index fetched via get_json_with_raw).
pub fn verify_bytes_sha1(bytes: &[u8], expected: &str) -> Result<(), NetError> {
    if expected.is_empty() {
        return Ok(());
    }
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(NetError::Parse(format!(
            "sha1 mismatch: expected {expected}, got {actual}"
        )))
    }
}

// unique-ish temp name next to `dest` so concurrent downloads (or retries)
// don't collide on one temp path.
fn tmp_download_path(dest: &Path) -> std::path::PathBuf {
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dest.with_file_name(format!("{file_name}.{unique}.part"))
}

async fn write_stream_to_file(
    response: reqwest::Response,
    tmp_dest: &Path,
    progress_cb: &impl Fn(u64, u64),
) -> Result<(), NetError> {
    use tokio::io::AsyncWriteExt;

    let total = response.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(tmp_dest).await?;
    let mut downloaded: u64 = 0;
    let mut stream = response;

    while let Some(chunk) = stream.chunk().await? {
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        progress_cb(downloaded, total);
    }
    file.flush().await?;

    Ok(())
}

// timeouts/connect/body errors and 5xx are worth retrying; a 404 or a
// parse error isn't (the body already arrived, so retrying won't help).
// 429 (rate limited) retries too: the client-side limiter only budgets
// this process, so a shared IP (other launchers, VPN exit nodes) can
// still trip the server-side limit — back off and try again.
fn is_retryable(err: &NetError) -> bool {
    match err {
        NetError::Http(e) => e.is_timeout() || e.is_body() || e.is_connect(),
        NetError::StatusError { status, .. } => *status >= 500 || *status == 429,
        // parse failures are deterministic — retrying won't help
        NetError::BadJson { .. } => false,
        _ => false,
    }
}

// tries JAVA_HOME first, then PATH, then just yolos "java" and hopes for the best
#[must_use]
pub fn detect_java_path() -> String {
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let java_name = if cfg!(windows) { "java.exe" } else { "java" };
        let bin = std::path::Path::new(&java_home).join("bin").join(java_name);
        if bin.exists() {
            tracing::trace!("Detected Java from JAVA_HOME: {}", bin.display());
            return bin.to_string_lossy().to_string();
        }
        tracing::warn!(
            "JAVA_HOME is set to {}, but {} does not exist",
            java_home,
            bin.display()
        );
    }
    match which::which("java") {
        Ok(path) => {
            tracing::trace!("Detected Java from PATH: {}", path.display());
            path.to_string_lossy().to_string()
        }
        Err(e) => {
            tracing::warn!(
                "Could not find java on PATH, falling back to literal 'java': {}",
                e
            );
            "java".to_string()
        }
    }
}

// every "java" on PATH (multiple JDKs layer in via update-alternatives,
// sdkman, etc.), plus JAVA_HOME's if set. feeds the settings UI's picker.
#[must_use]
pub fn discover_java_candidates() -> Vec<String> {
    let mut candidates = Vec::new();

    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let java_name = if cfg!(windows) { "java.exe" } else { "java" };
        let bin = std::path::Path::new(&java_home).join("bin").join(java_name);
        if bin.exists() {
            candidates.push(bin.to_string_lossy().to_string());
        }
    }

    if let Ok(paths) = which::which_all("java") {
        for path in paths {
            let path_str = path.to_string_lossy().to_string();
            if !candidates.contains(&path_str) {
                candidates.push(path_str);
            }
        }
    }

    candidates
}

// a discovered java runtime for the settings picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaCandidate {
    // path we'd actually launch with (whatever discover_java_candidates found:
    // could be a symlink, a shim, whatever — launching through it is fine)
    pub path: String,
    // human-readable version string, e.g. "17.0.9", when we could detect one
    pub version: Option<String>,
    // true when this is a runtime alloy itself downloaded into
    // <data_dir>/alloy/bin (see java_provision::runtime_install_dir), as
    // opposed to one alloy just found already installed on the system.
    // lets the picker label these distinctly ("(alloy)") instead of
    // showing a bare, unexplained path under the data dir.
    pub provisioned: bool,
}

// discover_java_candidates plus common per-OS install dirs plus alloy's own
// previously-provisioned runtimes, deduped by real path (symlinks all point
// at one java), with versions for display.
#[must_use]
pub fn discover_java_installations() -> Vec<JavaCandidate> {
    let mut raw: Vec<(String, bool)> = discover_java_candidates()
        .into_iter()
        .chain(scan_common_java_dirs())
        .map(|path| (path, false))
        .collect();
    raw.extend(scan_provisioned_java_dirs().into_iter().map(|path| (path, true)));

    dedup_tagged_by_real_path(raw)
        .into_iter()
        .map(|(path, provisioned)| {
            let version = detect_java_version(&path);
            JavaCandidate {
                path,
                version,
                provisioned,
            }
        })
        .collect()
}

// every runtime alloy has already downloaded into <data_dir>/alloy/bin —
// one subdir per (image_type, feature_version) combo (see
// java_provision::runtime_install_dir). these are real, launchable
// installs, but they live outside PATH/JAVA_HOME/the OS's usual install
// dirs, so discover_java_candidates/scan_common_java_dirs never find them
// on their own — the java picker would otherwise show "no runtimes found"
// for a fresh instance whose only java is one alloy fetched for it.
fn scan_provisioned_java_dirs() -> Vec<String> {
    let bin_root = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("alloy")
        .join("bin");

    let Ok(entries) = std::fs::read_dir(&bin_root) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        // each entry here is a runtime_install_dir (e.g. `jre-25`), not the
        // java home itself — adoptium archives unpack into a nested
        // top-level dir like `jdk-25.0.4+7-jre` inside it, so use the same
        // walk-a-level-down lookup java_provision uses at launch time
        // instead of assuming a fixed `bin/java` layout right under here.
        .filter_map(|entry| crate::net::java_provision::find_java_binary(&entry.path()))
        .collect()
}

// the handful of places JDKs live outside PATH/JAVA_HOME: linux system
// dirs, macOS's per-JVM layout, windows installer targets.
fn scan_common_java_dirs() -> Vec<String> {
    let mut found = Vec::new();
    let java_name = if cfg!(windows) { "java.exe" } else { "java" };

    let mut bases: Vec<std::path::PathBuf> = Vec::new();
    if cfg!(target_os = "linux") {
        bases.push("/usr/lib/jvm".into());
        bases.push("/opt/jdk".into());
        bases.push("/opt/java".into());
    } else if cfg!(target_os = "macos") {
        bases.push("/Library/Java/JavaVirtualMachines".into());
        if let Some(home) = dirs::home_dir() {
            bases.push(home.join("Library/Java/JavaVirtualMachines"));
        }
    } else if cfg!(windows) {
        for env_var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(pf) = std::env::var(env_var) {
                bases.push(std::path::PathBuf::from(&pf).join("Java"));
                bases.push(std::path::PathBuf::from(&pf).join("Eclipse Adoptium"));
                bases.push(std::path::PathBuf::from(&pf).join("Microsoft"));
            }
        }
    }

    for base in bases {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            // macOS bundles put the actual home under Contents/Home
            let candidates = [
                entry.path().join("bin").join(java_name),
                entry
                    .path()
                    .join("Contents")
                    .join("Home")
                    .join("bin")
                    .join(java_name),
            ];
            for bin in candidates {
                if bin.exists() {
                    found.push(bin.to_string_lossy().to_string());
                }
            }
        }
    }

    found
}

// collapses symlink chains pointing at one binary (e.g. /usr/bin/java ->
// .../java-17-openjdk/bin/java), keeping the first, PATH-friendliest
// spelling. carries a caller-defined tag through the dedup (here: "is this
// alloy's own provisioned runtime") so the tag survives on whichever
// occurrence is kept.
fn dedup_tagged_by_real_path(paths: Vec<(String, bool)>) -> Vec<(String, bool)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (path, tag) in paths {
        let key = std::fs::canonicalize(&path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            out.push((path, tag));
        }
    }
    out
}

// `java -version` -> quoted version, e.g. "17.0.9". best-effort: any
// failure yields None and the UI shows the bare path.
fn detect_java_version(path: &str) -> Option<String> {
    let output = std::process::Command::new(path)
        .arg("-version")
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_java_version(&stderr).or_else(|| parse_java_version(&stdout))
}

fn parse_java_version(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.contains("version"))?;
    let start = line.find('"')?;
    let rest = &line[start + 1..];
    let end = rest.find('"')?;
    let version = &rest[..end];
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

// turns a version string ("17.0.9", legacy "1.8.0_392") into a comparable
// key; legacy becomes the same key as "8.0.392" so both schemes sort
// sanely against each other.
fn java_version_sort_key(version: &str) -> Vec<u32> {
    let parts: Vec<u32> = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<u32>().ok())
        .collect();
    match parts.as_slice() {
        [1, rest @ ..] if !rest.is_empty() => rest.to_vec(),
        _ => parts,
    }
}

fn parse_major_version(version: &str) -> Option<u32> {
    java_version_sort_key(version).first().copied()
}

// the one java-version parse every caller shares: raw `java -version`
// output -> major version, legacy "1.8.0_392" counting as 8. single impl
// matters — provisioning picks a java at create-time and launch re-checks
// it, so the two must never disagree.
#[must_use]
pub fn parse_java_major_version(text: &str) -> Option<u32> {
    // prefer the quoted version; some JVMs print it bare, so fall back to
    // everything from the first digit run.
    let Some(token) = parse_java_version(text).or_else(|| {
        let start = text.find(|c: char| c.is_ascii_digit())?;
        Some(text[start..].to_owned())
    }) else {
        return None;
    };
    parse_major_version(&token)
}

// newest installed java (by version) for the first-run default. runtimes
// we couldn't detect sort last — they might be ancient — and only win if
// they're the only candidate.
#[must_use]
pub fn best_installed_java() -> Option<String> {
    let mut candidates = discover_java_installations();
    candidates.sort_by(|a, b| match (&a.version, &b.version) {
        (Some(va), Some(vb)) => java_version_sort_key(vb).cmp(&java_version_sort_key(va)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    candidates.into_iter().next().map(|c| c.path)
}

// an installed java satisfying `required_major` (mirrors launch's check).
// picks the *closest* match, not the newest — old Forge/NeoForge get
// cranky with bleeding-edge runtimes. for Jdk, requires a sibling `javac`.
#[must_use]
pub fn compatible_installed_java(required_major: u32, image_type: ImageType) -> Option<String> {
    discover_java_installations()
        .into_iter()
        .filter_map(|c| {
            let major = c.version.as_deref().and_then(parse_major_version)?;
            if major < required_major {
                return None;
            }
            if image_type == ImageType::Jdk && !has_sibling_javac(&c.path) {
                return None;
            }
            Some((major, c.path))
        })
        .min_by_key(|(major, _)| *major)
        .map(|(_, path)| path)
}

fn has_sibling_javac(java_path: &str) -> bool {
    let javac_name = if cfg!(windows) { "javac.exe" } else { "javac" };
    std::path::Path::new(java_path)
        .parent()
        .map(|dir| dir.join(javac_name).is_file())
        .unwrap_or(false)
}

// maven coord "org.example:artifact:1.0" -> fs path
// "org/example/artifact/1.0/artifact-1.0.jar"; optional 4th classifier.
#[must_use]
pub fn maven_coord_to_path(coord: &str) -> Option<String> {
    let parts: Vec<&str> = coord.split(':').collect();
    match parts.as_slice() {
        [group, artifact, version] => {
            let group_path = group.replace('.', "/");
            Some(format!(
                "{}/{}/{}/{}-{}.jar",
                group_path, artifact, version, artifact, version
            ))
        }
        [group, artifact, version, classifier] => {
            let group_path = group.replace('.', "/");
            Some(format!(
                "{}/{}/{}/{}-{}-{}.jar",
                group_path, artifact, version, artifact, version, classifier
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_version_sort_key_modern_scheme() {
        assert_eq!(java_version_sort_key("17.0.9"), vec![17, 0, 9]);
        assert_eq!(java_version_sort_key("21"), vec![21]);
    }

    #[test]
    fn java_version_sort_key_legacy_scheme() {
        // "1.8.0_392" should compare as major version 8, not 1
        assert_eq!(java_version_sort_key("1.8.0_392"), vec![8, 0, 392]);
    }

    #[test]
    fn java_version_sort_key_orders_newest_first_when_reversed() {
        let mut versions = vec!["1.8.0_392", "17.0.9", "11.0.2", "21"];
        versions.sort_by(|a, b| java_version_sort_key(b).cmp(&java_version_sort_key(a)));
        assert_eq!(versions, vec!["21", "17.0.9", "11.0.2", "1.8.0_392"]);
    }

    #[test]
    fn parse_major_version_modern_and_legacy() {
        assert_eq!(parse_major_version("17.0.9"), Some(17));
        assert_eq!(parse_major_version("1.8.0_392"), Some(8));
        assert_eq!(parse_major_version("garbage"), None);
    }

    #[test]
    fn maven_3_part_coord() {
        assert_eq!(
            maven_coord_to_path("org.example:artifact:1.0"),
            Some("org/example/artifact/1.0/artifact-1.0.jar".to_string())
        );
    }

    #[test]
    fn maven_4_part_coord_with_classifier() {
        assert_eq!(
            maven_coord_to_path("org.example:artifact:1.0:sources"),
            Some("org/example/artifact/1.0/artifact-1.0-sources.jar".to_string())
        );
    }

    #[test]
    fn maven_nested_group() {
        assert_eq!(
            maven_coord_to_path("com.google.code.gson:gson:2.10"),
            Some("com/google/code/gson/gson/2.10/gson-2.10.jar".to_string())
        );
    }

    #[test]
    fn maven_invalid_too_few_parts() {
        assert_eq!(maven_coord_to_path("org.example:artifact"), None);
    }

    #[test]
    fn parse_java_major_quoted_modern() {
        assert_eq!(
            parse_java_major_version("openjdk version \"25.0.3\" 2026-04-21"),
            Some(25)
        );
        assert_eq!(
            parse_java_major_version("openjdk version \"21.0.11\" 2026-04-21"),
            Some(21)
        );
    }

    #[test]
    fn parse_java_major_legacy_quoted() {
        assert_eq!(
            parse_java_major_version("java version \"1.8.0_402\""),
            Some(8)
        );
    }

    #[test]
    fn parse_java_major_unquoted_falls_back_to_digit_run() {
        // some JVMs print the version without quotes
        assert_eq!(
            parse_java_major_version("openjdk version 25.0.3 2026-04-21"),
            Some(25)
        );
        assert_eq!(
            parse_java_major_version("java version 1.8.0_402"),
            Some(8)
        );
    }

    #[test]
    fn parse_java_major_garbage_is_none() {
        assert_eq!(parse_java_major_version("command not found"), None);
        assert_eq!(parse_java_major_version(""), None);
    }

    #[test]
    fn parse_java_version_openjdk() {
        let stderr = "openjdk version \"17.0.9\" 2023-10-17\nOpenJDK Runtime Environment";
        assert_eq!(parse_java_version(stderr).as_deref(), Some("17.0.9"));
    }

    #[test]
    fn parse_java_version_oracle_jdk() {
        let stderr = "java version \"1.8.0_392\"\nJava(TM) SE Runtime Environment";
        assert_eq!(parse_java_version(stderr).as_deref(), Some("1.8.0_392"));
    }

    #[test]
    fn parse_java_version_no_match() {
        assert_eq!(parse_java_version("command not found"), None);
    }

    #[test]
    #[cfg(unix)]
    fn dedup_tagged_by_real_path_collapses_symlink_and_target() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("java-real");
        std::fs::write(&real, b"").unwrap();
        let link = tmp.path().join("java-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let paths = vec![
            (real.to_string_lossy().to_string(), false),
            (link.to_string_lossy().to_string(), true),
        ];
        let deduped = dedup_tagged_by_real_path(paths);
        assert_eq!(deduped.len(), 1);
        // first occurrence wins, tag included - the real path was listed
        // first here, so its (non-provisioned) tag is what survives.
        assert!(!deduped[0].1);
    }

    #[test]
    fn dedup_tagged_by_real_path_keeps_distinct_missing_paths() {
        let paths = vec![
            ("/nonexistent/a/java".to_string(), false),
            ("/nonexistent/b/java".to_string(), true),
        ];
        let deduped = dedup_tagged_by_real_path(paths);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn maven_invalid_too_many_parts() {
        assert_eq!(maven_coord_to_path("a:b:c:d:e"), None);
    }

    #[test]
    fn maven_invalid_single_part() {
        assert_eq!(maven_coord_to_path("just-a-string"), None);
    }

    #[test]
    fn bad_json_is_not_retryable_and_named_serde_error_survives() {
        let err = is_retryable(&NetError::BadJson {
            url: "https://api.modrinth.com/v2/project/x".into(),
            status: 200,
            snippet: "application/json: {...} — invalid type: null, expected a string".into(),
        });
        assert!(!err, "parse failures are deterministic and must not retry");
    }

    #[test]
    fn maven_empty_string() {
        assert_eq!(maven_coord_to_path(""), None);
    }

    #[test]
    fn rate_limited_hosts_match_api_endpoints_only() {
        assert_eq!(
            rate_limited_host("https://api.modrinth.com/v2/search?query=x"),
            Some(MODRINTH_API_HOST)
        );
        assert_eq!(
            rate_limited_host("https://api.curseforge.com/v1/mods/search"),
            Some(CURSEFORGE_API_HOST)
        );
        // CDNs and everything else stay ungated
        assert_eq!(
            rate_limited_host("https://cdn.modrinth.com/data/xyz/versions/1.0/mod.jar"),
            None
        );
        assert_eq!(
            rate_limited_host("https://mediafilez.forgecdn.org/files/1/2/file.jar"),
            None
        );
        assert_eq!(rate_limited_host("https://piston-meta.mojang.com/mc/game.json"), None);
        // ports and userinfo are stripped before matching
        assert_eq!(rate_limited_host("https://api.modrinth.com:443/v2/search"), Some(MODRINTH_API_HOST));
        assert_eq!(rate_limited_host("https://user@api.curseforge.com/v1/mods"), Some(CURSEFORGE_API_HOST));
        // a host that merely *ends with* the API host must not match
        assert_eq!(rate_limited_host("https://api.modrinth.com.evil.com/v2/search"), None);
        assert_eq!(rate_limited_host("not a url"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limiter_queues_requests_past_two_per_second() {
        let limiter = RateLimiter::new();
        let start = tokio::time::Instant::now();
        for _ in 0..5 {
            limiter.acquire().await;
        }
        // 2 immediate, 3rd+4th admitted at +1s, 5th at +2s. paused tokio
        // clock makes this deterministic and instant in wall time.
        assert!(
            start.elapsed() >= std::time::Duration::from_secs(2),
            "expected the 5-request burst to be queued, took {:?}",
            start.elapsed()
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "expected no over-throttling beyond the sliding window, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limiter_admits_bursts_up_to_the_cap_immediately() {
        let limiter = RateLimiter::new();
        let start = tokio::time::Instant::now();
        limiter.acquire().await;
        limiter.acquire().await;
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    }
}
