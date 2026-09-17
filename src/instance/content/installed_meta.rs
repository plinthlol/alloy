// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// tracks which catalog project (Modrinth/CurseForge, see ModpackHit::source_key)
// is responsible for which installed filename in an instance. mod/resourcepack
// filenames aren't stable across versions (e.g. sodium-fabric-0.5.jar ->
// sodium-fabric-0.6.jar), so without this there's no way to tell "this hit is
// already installed" or to replace the old file on reinstall instead of just
// adding a second copy alongside it.
//
// stored as a small hidden JSON sidecar at the instance root (the .minecraft
// dir) so mods and resource packs share one record and the file doesn't sit
// inside the content folders users actually look at. callers hand us the
// content dir they're working with; the root is its parent. sidecars written
// directly into a content dir by older builds are still read (and merged into
// the root file on the next write) so existing badges survive the move.
//
// it's filtered out of every content scan automatically since those only look
// at files matching a specific extension (.jar/.zip), never this one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const META_FILENAME: &str = ".alloy-installed.json";

// the instance root that owns the sidecar for `content_dir`
// (.minecraft/mods or .minecraft/resourcepacks -> .minecraft).
fn root_dir(content_dir: &Path) -> &Path {
    content_dir.parent().unwrap_or(content_dir)
}

fn sidecar_path(content_dir: &Path) -> PathBuf {
    root_dir(content_dir).join(META_FILENAME)
}

// where older builds kept the sidecar: directly in the content dir.
fn legacy_path(content_dir: &Path) -> PathBuf {
    content_dir.join(META_FILENAME)
}

fn read_map(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).unwrap_or_default())
        .unwrap_or_default()
}

fn write_map(path: &Path, map: &HashMap<String, String>) {
    if let Ok(json) = serde_json::to_string_pretty(map) {
        if let Err(e) = std::fs::write(path, json) {
            tracing::warn!(
                "Failed to write installed-content metadata at {}: {}",
                path.display(),
                e
            );
        }
    }
}

// missing/unreadable/corrupt file just means "nothing tracked yet" rather
// than an error - this is best-effort bookkeeping, not a source of truth
// (the actual files on disk are).
pub fn load(content_dir: &Path) -> HashMap<String, String> {
    let mut map = read_map(&legacy_path(content_dir));
    // the root sidecar wins where keys overlap (it's the newer record).
    map.extend(read_map(&sidecar_path(content_dir)));
    map
}

// if `file_path` lives in a tracked content dir (mods/, resourcepacks/
// under some instance root), returns that content dir plus the file's
// stem — exactly the inputs `remove_stems` needs. untracked locations
// (worlds, screenshots, logs, ...) yield None. the "disabled" rename
// variant matches too: "sodium-0.6.jar.disabled" reports stem
// "sodium-0.6" so deleting a toggled-off mod still clears its record.
#[must_use]
pub fn content_dir_and_stem(file_path: &Path) -> Option<(PathBuf, String)> {
    let content_dir = file_path.parent()?;
    let dir_name = content_dir.file_name()?.to_string_lossy().into_owned();
    if !matches!(dir_name.as_str(), "mods" | "resourcepacks") {
        return None;
    }
    let file_name = file_path.file_name()?.to_string_lossy().into_owned();
    for ext in ["jar", "zip", "jar.disabled", "zip.disabled"] {
        if let Some(stem) = file_name.strip_suffix(&format!(".{ext}")) {
            return Some((content_dir.to_path_buf(), stem.to_string()));
        }
    }
    None
}

// records that `key` is now installed as `filename`, returning the
// previously-recorded filename for that key if it differed (so the caller
// can delete the stale file left over from an older version).
pub fn record(content_dir: &Path, key: &str, filename: &str) -> Option<String> {
    let path = sidecar_path(content_dir);
    let mut map = read_map(&path);
    let previous = map.insert(key.to_string(), filename.to_string());

    // migrate: fold any legacy per-dir sidecar into the root record and
    // remove it, so the old location doesn't linger as a stale second copy.
    let legacy = legacy_path(content_dir);
    if legacy != path {
        for (k, v) in read_map(&legacy) {
            map.entry(k).or_insert(v);
        }
        if let Err(e) = std::fs::remove_file(&legacy) {
            tracing::debug!("No legacy sidecar to clean up at {}: {}", legacy.display(), e);
        }
    }

    write_map(&path, &map);

    previous.filter(|old| old != filename)
}

// drops every entry whose recorded filename corresponds to one of the
// deleted stems, so a mod removed outside an install (user delete key,
// rm, another launcher) stops showing the browse popup's "installed"
// badge for a file that no longer exists. a stem matches when the
// recorded filename is exactly the stem or starts with "<stem>." —
// covering the recorded enabled name, any extension, and the
// ".disabled"-suffixed variant of each. partial-name collisions are
// avoided on purpose: stem "sodium" must not match "sodium-0.6.jar".
// a no-op (no file created) when there's nothing recorded; removals
// only touch the root sidecar, since legacy per-dir sidecars are folded
// into it on the next record() anyway.
pub fn remove_stems(content_dir: &Path, stems: &[String]) {
    if stems.is_empty() {
        return;
    }
    let path = sidecar_path(content_dir);
    let mut map = read_map(&path);
    let before = map.len();
    map.retain(|_, filename| {
        !stems.iter().any(|stem| {
            filename == stem
                || filename
                    .strip_prefix(stem.as_str())
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    });
    if map.len() != before {
        write_map(&path, &map);
    }
}

// batch counterpart to record(): one read-modify-write per content dir
// instead of one per entry. modpack installs use this — they collect
// (project, filename) pairs from parallel download tasks first and record
// once afterwards, because concurrent record() calls would race on the
// sidecar file (each does an unlocked read-modify-write) and lose keys.
// entries are (content_dir, source_key, filename) triples; the last entry
// for a (dir, key) pair wins, mirroring record()'s insert semantics.
pub fn record_many(entries: &[(PathBuf, String, String)]) {
    if entries.is_empty() {
        return;
    }
    let mut order: Vec<PathBuf> = Vec::new();
    let mut grouped: HashMap<PathBuf, Vec<(String, String)>> = HashMap::new();
    for (dir, key, filename) in entries {
        if !grouped.contains_key(dir) {
            order.push(dir.clone());
        }
        grouped
            .entry(dir.clone())
            .or_default()
            .push((key.clone(), filename.clone()));
    }
    for dir in order {
        let path = sidecar_path(&dir);
        let mut map = read_map(&path);

        // same legacy migration record() performs
        let legacy = legacy_path(&dir);
        if legacy != path {
            for (k, v) in read_map(&legacy) {
                map.entry(k).or_insert(v);
            }
            if let Err(e) = std::fs::remove_file(&legacy) {
                tracing::debug!("No legacy sidecar to clean up at {}: {}", legacy.display(), e);
            }
        }

        for (key, filename) in &grouped[&dir] {
            map.insert(key.clone(), filename.clone());
        }
        write_map(&path, &map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // load()/record() take the content dir but must place the sidecar at
    // the instance root (its parent) and migrate legacy per-dir sidecars.

    #[test]
    fn record_writes_to_instance_root_not_content_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content_dir = tmp.path().join(".minecraft").join("mods");
        std::fs::create_dir_all(&content_dir).unwrap();

        record(&content_dir, "modrinth:abc", "sodium-0.6.jar");

        assert!(
            !content_dir.join(META_FILENAME).exists(),
            "sidecar must not sit in the content dir"
        );
        let map = load(&content_dir);
        assert_eq!(map.get("modrinth:abc").map(String::as_str), Some("sodium-0.6.jar"));
    }

    #[test]
    fn record_returns_previous_filename_for_same_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content_dir = tmp.path().join(".minecraft").join("resourcepacks");
        std::fs::create_dir_all(&content_dir).unwrap();

        assert_eq!(record(&content_dir, "curseforge:1", "pack-v1.zip"), None);
        assert_eq!(
            record(&content_dir, "curseforge:1", "pack-v2.zip"),
            Some("pack-v1.zip".to_string())
        );
    }

    #[test]
    fn legacy_sidecar_is_merged_and_removed_on_next_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content_dir = tmp.path().join(".minecraft").join("mods");
        std::fs::create_dir_all(&content_dir).unwrap();

        // simulate an older build's sidecar in the content dir
        let legacy_json = r#"{"modrinth:old": "old.jar"}"#;
        std::fs::write(content_dir.join(META_FILENAME), legacy_json).unwrap();

        // a fresh install lands in the root file and migrates the old one
        record(&content_dir, "modrinth:new", "new.jar");

        assert!(!content_dir.join(META_FILENAME).exists(), "legacy file removed");
        let map = load(&content_dir);
        assert_eq!(map.get("modrinth:old").map(String::as_str), Some("old.jar"));
        assert_eq!(map.get("modrinth:new").map(String::as_str), Some("new.jar"));
    }

    #[test]
    fn record_many_writes_all_entries_and_migrates_legacy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mods = tmp.path().join(".minecraft").join("mods");
        let packs = tmp.path().join(".minecraft").join("resourcepacks");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::create_dir_all(&packs).unwrap();

        // a legacy per-dir sidecar that must migrate on the batch write
        std::fs::write(mods.join(META_FILENAME), r#"{"modrinth:old": "old.jar"}"#).unwrap();

        record_many(&[
            (mods.clone(), "modrinth:abc".into(), "sodium-0.6.jar".into()),
            (mods.clone(), "modrinth:def".into(), "gravity-1.2.jar".into()),
            (packs.clone(), "curseforge:9".into(), "fancy.zip".into()),
        ]);

        assert!(!mods.join(META_FILENAME).exists(), "legacy sidecar migrated away");
        let mods_map = load(&mods);
        assert_eq!(mods_map.get("modrinth:abc").map(String::as_str), Some("sodium-0.6.jar"));
        assert_eq!(mods_map.get("modrinth:def").map(String::as_str), Some("gravity-1.2.jar"));
        assert_eq!(mods_map.get("modrinth:old").map(String::as_str), Some("old.jar"));
        assert_eq!(
            load(&packs).get("curseforge:9").map(String::as_str),
            Some("fancy.zip")
        );
    }

    #[test]
    fn record_many_is_a_noop_for_no_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mods = tmp.path().join(".minecraft").join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        record_many(&[]);
        assert!(!sidecar_path(&mods).exists());
    }

    #[test]
    fn content_dir_and_stem_matches_mods_and_resourcepacks() {
        let root = Path::new("/instances/pack/.minecraft");
        let (dir, stem) =
            content_dir_and_stem(&root.join("mods").join("sodium-0.6.jar")).unwrap();
        assert_eq!(dir, root.join("mods"));
        assert_eq!(stem, "sodium-0.6");

        let (dir, stem) = content_dir_and_stem(&root.join("resourcepacks").join("fancy.zip"))
            .unwrap();
        assert_eq!(dir, root.join("resourcepacks"));
        assert_eq!(stem, "fancy");
    }

    #[test]
    fn content_dir_and_stem_matches_disabled_variant_and_rejects_other_dirs() {
        let root = Path::new("/instances/pack/.minecraft");

        let (_, stem) = content_dir_and_stem(&root.join("mods").join("sodium-0.6.jar.disabled"))
            .unwrap();
        assert_eq!(stem, "sodium-0.6");

        // worlds/screenshots/logs/config files are never tracked
        assert!(content_dir_and_stem(&root.join("saves").join("world")).is_none());
        assert!(content_dir_and_stem(&root.join("mods").join("notes.txt")).is_none());
        assert!(content_dir_and_stem(&root.join("sodium-0.6.jar")).is_none());
    }

    #[test]
    fn remove_stems_drops_only_the_deleted_stems_record() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content_dir = tmp.path().join(".minecraft").join("mods");
        std::fs::create_dir_all(&content_dir).unwrap();

        record(&content_dir, "modrinth:abc", "sodium-0.6.jar");
        record(&content_dir, "modrinth:def", "gravity-1.2.jar");

        remove_stems(&content_dir, &["sodium-0.6".to_string()]);

        let map = load(&content_dir);
        assert!(!map.contains_key("modrinth:abc"), "deleted mod's record must go");
        assert_eq!(
            map.get("modrinth:def").map(String::as_str),
            Some("gravity-1.2.jar"),
            "unrelated mod's record must survive"
        );
    }

    #[test]
    fn remove_stems_needs_the_dot_so_partial_names_dont_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content_dir = tmp.path().join(".minecraft").join("mods");
        std::fs::create_dir_all(&content_dir).unwrap();

        record(&content_dir, "modrinth:abc", "sodium-0.6.jar");

        // deleting a file stemmed "sodium" (i.e. "sodium.jar") must not
        // touch the "sodium-0.6.jar" record
        remove_stems(&content_dir, &["sodium".to_string()]);
        assert!(load(&content_dir).contains_key("modrinth:abc"));
    }

    #[test]
    fn remove_stems_on_missing_sidecar_creates_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content_dir = tmp.path().join(".minecraft").join("mods");
        std::fs::create_dir_all(&content_dir).unwrap();

        remove_stems(&content_dir, &["whatever".to_string()]);

        assert!(!sidecar_path(&content_dir).exists());
    }

    #[test]
    fn load_prefers_root_record_over_stale_legacy_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content_dir = tmp.path().join(".minecraft").join("mods");
        std::fs::create_dir_all(&content_dir).unwrap();

        std::fs::write(
            content_dir.join(META_FILENAME),
            r#"{"modrinth:x": "legacy.jar"}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".minecraft").join(META_FILENAME),
            r#"{"modrinth:x": "current.jar"}"#,
        )
        .unwrap();

        let map = load(&content_dir);
        assert_eq!(map.get("modrinth:x").map(String::as_str), Some("current.jar"));
    }
}
