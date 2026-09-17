// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// modpack install from either catalog:
// - Modrinth: the pack's self-contained .mrpack (modrinth.index.json +
//   per-file download URLs); its manifest gives game version/loader.
// - CurseForge: a zip with manifest.json + overrides but no file URLs, so
//   each (projectID, fileID) pair gets resolved through the CF API.
// both end the same way: InstanceManager::create, then mods layered in,
// then the pack's overrides on top.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::instance::manager::{InstanceError, InstanceManager};
use crate::instance::models::{InstanceConfig, ModLoader};
use crate::net::curseforge;
use crate::net::modrinth::{self, ProjectVersion};
use crate::net::{HttpClient, NetError, download_file};

// pack mods are independent, so fan out the downloads instead of waiting
// on round-trip latency one at a time. bounded well below
// available_parallelism — this is I/O-bound and a big pool risks tripping
// Modrinth/CurseForge rate limits.
const DOWNLOAD_CONCURRENCY: usize = 8;

#[derive(Debug, Error)]
pub enum ModpackError {
    #[error(transparent)]
    Net(#[from] crate::net::NetError),
    #[error(transparent)]
    CurseForge(#[from] curseforge::CurseForgeError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("couldn't read modpack archive: {0}")]
    Zip(String),
    #[error("modpack manifest error: {0}")]
    Manifest(String),
    #[error("modpack doesn't declare a Minecraft version")]
    MissingGameVersion,
    #[error("the pack author has disabled third-party downloads for this file")]
    NoDownloadUrl,
    #[error(transparent)]
    Instance(#[from] InstanceError),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MrpackIndex {
    #[serde(default)]
    files: Vec<MrpackFile>,
    #[serde(default)]
    dependencies: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct MrpackFile {
    path: String,
    downloads: Vec<String>,
    #[serde(default)]
    env: Option<MrpackEnv>,
}

#[derive(Debug, Deserialize)]
struct MrpackEnv {
    #[serde(default)]
    client: String,
}

// modrinth dependency keys for the loader a pack needs. a pack should
// declare at most one alongside "minecraft", so check in a fixed order
// and take the first hit.
fn loader_from_dependencies(deps: &HashMap<String, String>) -> (ModLoader, Option<String>) {
    if let Some(v) = deps.get("forge") {
        (ModLoader::Forge, Some(v.clone()))
    } else if let Some(v) = deps.get("neoforge") {
        (ModLoader::NeoForge, Some(v.clone()))
    } else if let Some(v) = deps.get("fabric-loader") {
        (ModLoader::Fabric, Some(v.clone()))
    } else if let Some(v) = deps.get("quilt-loader") {
        (ModLoader::Quilt, Some(v.clone()))
    } else {
        (ModLoader::Vanilla, None)
    }
}

/// downloads `version`'s .mrpack, creates a base instance for its declared
/// game version/loader, then layers in the pack's mods and overrides.
/// `progress` gets short status strings the caller can surface like any
/// other install.
pub async fn install_from_modrinth(
    manager: &InstanceManager,
    name: &str,
    version: &ProjectVersion,
    mut progress: impl FnMut(&str),
) -> Result<InstanceConfig, ModpackError> {
    let client = HttpClient::shared();

    progress("Downloading modpack...");
    let tmp_dir = std::env::temp_dir().join(format!(
        "alloy-mrpack-{}-{}",
        std::process::id(),
        sanitize(&version.id)
    ));
    std::fs::create_dir_all(&tmp_dir)?;
    let mrpack_path = tmp_dir.join("pack.mrpack");
    modrinth::download_primary_file(&client, version, &mrpack_path, |_, _| {}).await?;

    progress("Reading modpack manifest...");
    let file = std::fs::File::open(&mrpack_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| ModpackError::Zip(e.to_string()))?;

    let index: MrpackIndex = {
        let mut entry = archive
            .by_name("modrinth.index.json")
            .map_err(|e| ModpackError::Manifest(format!("no modrinth.index.json: {e}")))?;
        let mut buf = String::new();
        entry.read_to_string(&mut buf)?;
        serde_json::from_str(&buf).map_err(|e| ModpackError::Manifest(e.to_string()))?
    };

    let game_version = index
        .dependencies
        .get("minecraft")
        .cloned()
        .ok_or(ModpackError::MissingGameVersion)?;
    let (loader, loader_version) = loader_from_dependencies(&index.dependencies);

    progress(&format!("Installing {loader} {game_version}..."));
    let instance = manager
        .create(name, &game_version, loader, loader_version.as_deref())
        .await?;

    let instance_dir = manager.instances_dir.join(name).join(".minecraft");

    // filter to the files we'll actually download so `total`/progress
    // reflects real work. entry.path comes straight from the pack's
    // modrinth.index.json, so it's untrusted - safe_join refuses any entry
    // that tries to write outside instance_dir (e.g. via `..`) instead of
    // letting a malicious pack plant files elsewhere on disk.
    let downloads: Vec<(String, std::path::PathBuf)> = index
        .files
        .into_iter()
        .filter(|entry| !matches!(&entry.env, Some(env) if env.client == "unsupported"))
        .filter_map(|entry| {
            let url = entry.downloads.into_iter().next()?;
            match safe_join(&instance_dir, &entry.path) {
                Some(dest) => Some((url, dest)),
                None => {
                    tracing::warn!(
                        "Skipping modpack file with unsafe path: {:?}",
                        entry.path
                    );
                    None
                }
            }
        })
        .collect();

    let total = downloads.len();
    progress(&format!("Downloading {total} mods..."));

    // sha1 per file so the batch hash lookup afterwards can map each
    // downloaded file back to its Modrinth (project, version) identity —
    // mrpack entries carry only URLs, and the installed-content sidecar
    // tracks project ids ("modrinth:<id>"), not file names. collected
    // through a mutex because download tasks own their dest path.
    let downloaded_hashes: Arc<Mutex<Vec<(PathBuf, String)>>> =
        Arc::new(Mutex::new(Vec::new()));

    let semaphore = Arc::new(Semaphore::new(DOWNLOAD_CONCURRENCY));
    let mut join_set: JoinSet<Result<(), NetError>> = JoinSet::new();
    for (url, dest) in downloads {
        let client = client.clone();
        let semaphore = semaphore.clone();
        let downloaded_hashes = downloaded_hashes.clone();
        join_set.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("download semaphore is never closed");
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            download_file(&client, &url, &dest, |_, _| {}).await?;
            // hash what we just wrote: modrinth.index.json carries no sha1
            // to compare against, this hash is purely the lookup key for
            // the version_files batch call below. a read failure just means
            // no identity record for this file — not worth failing the pack.
            if let Ok(bytes) = tokio::fs::read(&dest).await {
                if let Ok(mut hashes) = downloaded_hashes.lock() {
                    hashes.push((dest, sha1_hex(&bytes)));
                }
            }
            Ok(())
        });
    }

    let mut completed = 0usize;
    while let Some(result) = join_set.join_next().await {
        completed += 1;
        progress(&format!("Downloading mods... ({completed}/{total})"));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                join_set.abort_all();
                return Err(e.into());
            }
            Err(e) if e.is_panic() => {
                join_set.abort_all();
                std::panic::resume_unwind(e.into_panic());
            }
            Err(_) => {
                // task was cancelled (e.g. by our own abort_all from a
                // sibling task's failure) - nothing more to do with it.
            }
        }
    }

    // recover each file's Modrinth project identity via the batch hash
    // lookup and record it, so pack-installed mods show the "installed"
    // badge in the browse popup and get replaced (not duplicated) when a
    // newer version is installed later. purely bookkeeping: a failed
    // lookup leaves that mod untracked but the pack itself still installs.
    let hashes = downloaded_hashes
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    if !hashes.is_empty() {
        progress("Identifying installed mods...");
        let sha1s: Vec<String> = hashes.iter().map(|(_, h)| h.clone()).collect();
        match modrinth::lookup_versions_by_sha1(&client, &sha1s).await {
            Ok(version_by_hash) => {
                let entries: Vec<(PathBuf, String, String)> = hashes
                    .iter()
                    .filter_map(|(dest, hash)| {
                        let version = version_by_hash.get(hash)?;
                        let filename = dest.file_name()?.to_str()?.to_string();
                        let dir = dest.parent()?.to_path_buf();
                        Some((dir, format!("modrinth:{}", version.project_id), filename))
                    })
                    .collect();
                crate::instance::content::installed_meta::record_many(&entries);
            }
            Err(e) => {
                tracing::warn!(
                    "Modrinth hash lookup failed; pack mods won't be tracked as installed: {}",
                    e
                );
            }
        }
    }

    progress("Extracting overrides...");
    extract_prefixed(&mut archive, "overrides/", &instance_dir)?;
    // client-overrides stomp the generic ones — they exist to override on
    // client installs, so this pass runs second on purpose.
    extract_prefixed(&mut archive, "client-overrides/", &instance_dir)?;

    let _ = std::fs::remove_dir_all(&tmp_dir);

    Ok(instance)
}

// CurseForge's manifest.json differs from Modrinth's: files are
// (projectID, fileID) pairs resolved through the API rather than URLs, and
// the loader is one id string like "forge-47.2.0".
#[derive(Debug, Deserialize)]
struct CurseManifest {
    minecraft: CurseMinecraft,
    #[serde(default)]
    files: Vec<CurseManifestFile>,
    #[serde(default = "default_overrides_dir")]
    overrides: String,
}

fn default_overrides_dir() -> String {
    "overrides".to_string()
}

#[derive(Debug, Deserialize)]
struct CurseMinecraft {
    version: String,
    #[serde(default, rename = "modLoaders")]
    mod_loaders: Vec<CurseModLoaderEntry>,
}

#[derive(Debug, Deserialize)]
struct CurseModLoaderEntry {
    id: String,
    #[serde(default)]
    primary: bool,
}

#[derive(Debug, Deserialize, Clone, Copy)]
struct CurseManifestFile {
    #[serde(rename = "projectID")]
    project_id: u32,
    #[serde(rename = "fileID")]
    file_id: u32,
}

// CF modLoader ids are "<name>-<version>" (e.g. "forge-47.2.0"). picks
// the primary entry, or the first if none is marked (nothing enforces it).
fn loader_from_curseforge(mod_loaders: &[CurseModLoaderEntry]) -> (ModLoader, Option<String>) {
    let Some(entry) = mod_loaders.iter().find(|m| m.primary).or(mod_loaders.first()) else {
        return (ModLoader::Vanilla, None);
    };
    let (name, version) = entry.id.split_once('-').unwrap_or((entry.id.as_str(), ""));
    let loader = match name {
        "forge" => ModLoader::Forge,
        "neoforge" => ModLoader::NeoForge,
        "fabric" => ModLoader::Fabric,
        "quilt" => ModLoader::Quilt,
        _ => ModLoader::Vanilla,
    };
    let version = (!version.is_empty()).then(|| version.to_string());
    (loader, version)
}

/// downloads the pack zip (`file`), creates a base instance for its
/// declared version/loader, resolves every `files` entry through the CF
/// API to fetch each mod, then applies overrides. mirrors
/// [`install_from_modrinth`].
pub async fn install_from_curseforge(
    manager: &InstanceManager,
    name: &str,
    api_key: &str,
    file: &curseforge::ModFile,
    mut progress: impl FnMut(&str),
) -> Result<InstanceConfig, ModpackError> {
    let client = HttpClient::shared();

    progress("Downloading modpack...");
    let tmp_dir = std::env::temp_dir().join(format!(
        "alloy-cfpack-{}-{}",
        std::process::id(),
        file.id
    ));
    std::fs::create_dir_all(&tmp_dir)?;
    let pack_path = tmp_dir.join("pack.zip");
    let downloaded = curseforge::download_mod_file(&client, file, &pack_path, |_, _| {}).await?;
    if !downloaded {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(ModpackError::NoDownloadUrl);
    }

    progress("Reading modpack manifest...");
    let zip_file = std::fs::File::open(&pack_path)?;
    let mut archive =
        zip::ZipArchive::new(zip_file).map_err(|e| ModpackError::Zip(e.to_string()))?;

    let manifest: CurseManifest = {
        let mut entry = archive
            .by_name("manifest.json")
            .map_err(|e| ModpackError::Manifest(format!("no manifest.json: {e}")))?;
        let mut buf = String::new();
        entry.read_to_string(&mut buf)?;
        serde_json::from_str(&buf).map_err(|e| ModpackError::Manifest(e.to_string()))?
    };

    let game_version = manifest.minecraft.version.clone();
    let (loader, loader_version) = loader_from_curseforge(&manifest.minecraft.mod_loaders);

    progress(&format!("Installing {loader} {game_version}..."));
    let instance = manager
        .create(name, &game_version, loader, loader_version.as_deref())
        .await?;

    let instance_dir = manager.instances_dir.join(name).join(".minecraft");
    let mods_dir = instance_dir.join("mods");
    std::fs::create_dir_all(&mods_dir)?;

    let total = manifest.files.len();
    progress(&format!("Resolving and downloading {total} mods..."));

    // (content dir, project key, filename) triples gathered by the download
    // tasks, recorded into the installed-content sidecar once the pack is
    // in — the sidecar is what makes the browse popup show "installed" and
    // lets a later single-mod install replace the pack's copy instead of
    // adding a second one.
    let recorded_files: Arc<Mutex<Vec<(PathBuf, String, String)>>> =
        Arc::new(Mutex::new(Vec::new()));

    let semaphore = Arc::new(Semaphore::new(DOWNLOAD_CONCURRENCY));
    let mut join_set: JoinSet<Result<(), NetError>> = JoinSet::new();
    for entry in manifest.files {
        let client = client.clone();
        let semaphore = semaphore.clone();
        let api_key = api_key.to_owned();
        let mods_dir = mods_dir.clone();
        let recorded_files = recorded_files.clone();
        join_set.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("download semaphore is never closed");

            let mod_file =
                match curseforge::get_file(&client, &api_key, entry.project_id, entry.file_id)
                    .await
                {
                    Ok(f) => f,
                    Err(e) => {
                        // one mod failing to resolve shouldn't sink the pack
                        // — log it and move on (mrpack installs are just as
                        // tolerant).
                        tracing::warn!(
                            "Skipping CurseForge mod {}/{}: {}",
                            entry.project_id,
                            entry.file_id,
                            e
                        );
                        return Ok(());
                    }
                };

            // fileName is server-provided metadata from the CF API, not a
            // literal filesystem-safe value - guard it the same way as the
            // zip/mrpack paths above rather than trusting it.
            let Some(dest) = safe_join(&mods_dir, &mod_file.file_name) else {
                tracing::warn!(
                    "Skipping CurseForge file with unsafe name: {:?}",
                    mod_file.file_name
                );
                return Ok(());
            };
            let downloaded = curseforge::download_mod_file(&client, &mod_file, &dest, |_, _| {}).await?;
            if !downloaded {
                tracing::warn!(
                    "Skipping '{}' - author has disabled third-party downloads for this file",
                    mod_file.file_name
                );
            } else if let Some(parent) = dest.parent() {
                // mod_id is the CF project id the manifest's projectID
                // refers to, carried on every resolved file — same key shape
                // single-mod installs record ("curseforge:<project>").
                if let Ok(mut pending) = recorded_files.lock() {
                    pending.push((
                        parent.to_path_buf(),
                        format!("curseforge:{}", mod_file.mod_id),
                        mod_file.file_name.clone(),
                    ));
                }
            }
            Ok(())
        });
    }

    let mut completed = 0usize;
    while let Some(result) = join_set.join_next().await {
        completed += 1;
        progress(&format!("Resolving and downloading mods... ({completed}/{total})"));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                join_set.abort_all();
                return Err(ModpackError::from(e));
            }
            Err(e) if e.is_panic() => {
                join_set.abort_all();
                std::panic::resume_unwind(e.into_panic());
            }
            Err(_) => {
                // task was cancelled by our own abort_all from a sibling
                // task's failure - nothing more to do with it.
            }
        }
    }

    progress("Extracting overrides...");
    let overrides_prefix = format!("{}/", manifest.overrides.trim_end_matches('/'));
    extract_prefixed(&mut archive, &overrides_prefix, &instance_dir)?;

    if let Ok(recorded) = recorded_files.lock() {
        crate::instance::content::installed_meta::record_many(&recorded);
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);

    Ok(instance)
}

// lowercase hex sha1 of `bytes`, for Modrinth's version_files hash lookup.
fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn extract_prefixed(
    archive: &mut zip::ZipArchive<std::fs::File>,
    prefix: &str,
    dest_dir: &Path,
) -> Result<(), ModpackError> {
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| ModpackError::Zip(e.to_string()))?;
        let name = entry.name().to_string();
        if !name.starts_with(prefix) || name.ends_with('/') {
            continue;
        }
        let rel = &name[prefix.len()..];
        if rel.is_empty() {
            continue;
        }
        // zip entry names are attacker-controlled (any pack author can put
        // whatever they want in the archive) - a `..`-laden name here is
        // the classic "zip slip" trick for writing outside dest_dir, so
        // route through safe_join instead of a bare .join() and skip
        // anything that tries it.
        let Some(dest) = safe_join(dest_dir, rel) else {
            tracing::warn!("Skipping zip entry with unsafe path: {:?}", name);
            continue;
        };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        std::fs::write(dest, buf)?;
    }
    Ok(())
}

// joins `rel` onto `base`, refusing anything that could escape `base` -
// absolute paths, `..` components, or (on Windows) drive prefixes. pack
// contents (mrpack file paths, zip entry names, CurseForge file names) are
// untrusted input from third-party pack authors/APIs, so every path we
// derive from them and write to disk goes through this instead of a plain
// `.join()`. returns None for anything unsafe; callers skip that entry
// rather than aborting the whole install, matching how a single bad
// CurseForge resolve is already just logged and skipped.
fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    let mut result = base.to_path_buf();
    for component in rel_path.components() {
        match component {
            Component::Normal(seg) => result.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(result)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_allows_normal_relative_paths() {
        let base = Path::new("/instances/pack/.minecraft");
        assert_eq!(
            safe_join(base, "mods/example.jar").unwrap(),
            base.join("mods/example.jar")
        );
        assert_eq!(
            safe_join(base, "config/sub/dir/file.toml").unwrap(),
            base.join("config/sub/dir/file.toml")
        );
    }

    #[test]
    fn safe_join_rejects_parent_dir_traversal() {
        let base = Path::new("/instances/pack/.minecraft");
        assert!(safe_join(base, "../../../../etc/passwd").is_none());
        assert!(safe_join(base, "mods/../../escaped.jar").is_none());
        assert!(safe_join(base, "..").is_none());
    }

    #[test]
    fn safe_join_rejects_absolute_paths() {
        let base = Path::new("/instances/pack/.minecraft");
        assert!(safe_join(base, "/etc/passwd").is_none());
    }

    #[test]
    fn safe_join_allows_current_dir_components() {
        let base = Path::new("/instances/pack/.minecraft");
        assert_eq!(
            safe_join(base, "./mods/./example.jar").unwrap(),
            base.join("mods/example.jar")
        );
    }

    #[test]
    fn loader_from_dependencies_picks_forge() {
        let mut deps = HashMap::new();
        deps.insert("minecraft".to_string(), "1.20.1".to_string());
        deps.insert("forge".to_string(), "47.2.0".to_string());
        let (loader, version) = loader_from_dependencies(&deps);
        assert_eq!(loader, ModLoader::Forge);
        assert_eq!(version.as_deref(), Some("47.2.0"));
    }

    #[test]
    fn loader_from_dependencies_falls_back_to_vanilla() {
        let mut deps = HashMap::new();
        deps.insert("minecraft".to_string(), "1.20.1".to_string());
        let (loader, version) = loader_from_dependencies(&deps);
        assert_eq!(loader, ModLoader::Vanilla);
        assert_eq!(version, None);
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize("a/b c.d"), "a_b_c_d");
    }

    #[test]
    fn sha1_hex_matches_known_digest() {
        // sha1("abc") per FIPS 180-1
        assert_eq!(
            sha1_hex(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn loader_from_curseforge_parses_forge() {
        let loaders = vec![CurseModLoaderEntry {
            id: "forge-47.2.0".to_string(),
            primary: true,
        }];
        let (loader, version) = loader_from_curseforge(&loaders);
        assert_eq!(loader, ModLoader::Forge);
        assert_eq!(version.as_deref(), Some("47.2.0"));
    }

    #[test]
    fn loader_from_curseforge_prefers_primary_entry() {
        let loaders = vec![
            CurseModLoaderEntry {
                id: "forge-47.2.0".to_string(),
                primary: false,
            },
            CurseModLoaderEntry {
                id: "fabric-0.15.11".to_string(),
                primary: true,
            },
        ];
        let (loader, version) = loader_from_curseforge(&loaders);
        assert_eq!(loader, ModLoader::Fabric);
        assert_eq!(version.as_deref(), Some("0.15.11"));
    }

    #[test]
    fn loader_from_curseforge_falls_back_to_vanilla_when_empty() {
        let (loader, version) = loader_from_curseforge(&[]);
        assert_eq!(loader, ModLoader::Vanilla);
        assert_eq!(version, None);
    }
}
