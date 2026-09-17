// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// generic scrollable list for content items (mods, resource packs, shaders, worlds).
// supports toggling items on/off by renaming files with .disabled suffix,
// search filtering, per-instance caching, and directory change detection.
// also handles minecraft's formatting codes for colored mod names/descriptions
// because apparently mojang thought terminal UIs would need that. thanks guys

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use ratatui_image::{CropOptions, Resize, StatefulImage, protocol::StatefulProtocol};
use tui_widget_list::{ListBuilder, ListState as TuiListState, ListView};

use crate::config::theme::THEME;
use crate::instance::content::mods::{ContentEntry, IconCell};

type ScanOneFn = fn(&Path, &str, bool) -> ContentEntry;

const IMAGE_REBUILD_PER_TICK: usize = 32;

// per-instance snapshot kept across switches (see ContentListState::cache):
// the scanned entries plus the UI position and the already-decoded icon
// protocols. keeping the protocols with the entries means switching back to
// an instance we were just on doesn't re-decode every icon from scratch
// (and re-download nothing, since icons come off local disk).
struct CachedList {
    entries: Vec<ContentEntry>,
    selected: Option<usize>,
    image_protocols: HashMap<String, StatefulProtocol>,
    decoded_images: HashMap<String, image::DynamicImage>,
}

struct PendingContentImage {
    file_stem: String,
    path: std::path::PathBuf,
    icon_lines: Vec<Vec<IconCell>>,
    image: Option<image::DynamicImage>,
}

struct DisplayMetadata {
    description: String,
    has_description: bool,
}

// result from the notify-triggered background diff
struct WatcherDiff {
    toggled: Vec<(String, bool, std::path::PathBuf)>,
    removed: Vec<String>,
    added: Vec<ContentEntry>,
    // stems that kept the same path/enabled state but whose content
    // signature changed in place (e.g. icon.png/pack.png overwritten inside
    // an existing world/resource pack directory) — these get rescanned so
    // the new icon/metadata actually shows up without a restart.
    changed: Vec<ContentEntry>,
}

pub struct ContentListState {
    pub entries: Vec<ContentEntry>,
    pub list_state: TuiListState,
    pub scrollbar_state: ScrollbarState,
    pub loaded_for: Option<String>,
    pub loading: bool,
    image_protocols: HashMap<String, StatefulProtocol>,
    decoded_images: HashMap<String, image::DynamicImage>,
    requested_images: HashSet<String>,
    pending_images: Arc<Mutex<Vec<PendingContentImage>>>,
    images_dirty: bool,
    // caps how many icon decode/resize tasks run at once (see
    // request_image_loads) so a big local mod list doesn't oversubscribe
    // the CPU competing with the concurrent content scan.
    image_load_semaphore: Arc<tokio::sync::Semaphore>,
    // Sixel only: the icon Rect actually drawn for each file_stem last
    // frame, so render_image_icons can tell whether a row's icon moved,
    // resized, or vanished since the previous frame and only clear the
    // cells that need it (see render_image_icons for why this matters).
    sixel_drawn_rects: HashMap<String, Rect>,
    display_metadata: HashMap<String, DisplayMetadata>,
    pub search: crate::tui::widgets::search::SearchState,
    cache: HashMap<String, CachedList>,
    // streaming: individual entries arrive here during initial load
    stream_rx: Option<mpsc::Receiver<ContentEntry>>,
    // file watcher: notify callback spawns background work,
    // precomputed diff lands here for the UI to pick up
    watcher_diff: Arc<Mutex<Option<WatcherDiff>>>,
    _watcher: Option<notify::RecommendedWatcher>,
    watched_dir: Option<std::path::PathBuf>,
    // stored for the watcher to scan individual new files
    scan_one_fn: Option<ScanOneFn>,
    content_ext: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct PendingContentDelete {
    pub name: String,
    pub path: std::path::PathBuf,
}

impl Default for ContentListState {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            list_state: TuiListState::default(),
            scrollbar_state: ScrollbarState::default(),
            loaded_for: None,
            loading: false,
            image_protocols: HashMap::new(),
            decoded_images: HashMap::new(),
            requested_images: HashSet::new(),
            pending_images: Arc::new(Mutex::new(Vec::new())),
            images_dirty: true,
            image_load_semaphore: Arc::new(tokio::sync::Semaphore::new(
                std::thread::available_parallelism()
                    .map(std::num::NonZeroUsize::get)
                    .unwrap_or(4),
            )),
            sixel_drawn_rects: HashMap::new(),
            display_metadata: HashMap::new(),
            search: crate::tui::widgets::search::SearchState::default(),
            cache: HashMap::new(),
            stream_rx: None,
            watcher_diff: Arc::new(Mutex::new(None)),
            _watcher: None,
            watched_dir: None,
            scan_one_fn: None,
            content_ext: None,
        }
    }
}

impl ContentListState {
    // drop any cached/loaded state tied to `name`, so the next render for
    // whichever instance is now selected forces a fresh directory scan
    // instead of reusing stale data left over from before a rename.
    pub fn invalidate(&mut self, name: &str) {
        if self.loaded_for.as_deref() == Some(name) {
            self.loaded_for = None;
        }
        self.cache.remove(name);
    }

    // true when the selection is already at (or before) the first row, i.e.
    // pressing "up" again wouldn't move it. used to trigger instance rename
    // from the content header when the user keeps pressing k/Up past the top.
    pub fn is_at_top(&self) -> bool {
        self.list_state.selected.is_none_or(|s| s == 0)
    }

    pub fn request_image_loads(&mut self, picker: &ratatui_image::picker::Picker) {
        if !self.images_dirty {
            return;
        }
        self.images_dirty = false;
        let mut rebuilt = 0usize;

        let use_image_protocol =
            picker.protocol_type() != ratatui_image::picker::ProtocolType::Halfblocks;
        let use_quadrants = crate::config::SETTINGS.ui.image_protocol
            == crate::config::settings::ImageProtocol::Quadrants;
        if !use_image_protocol {
            self.image_protocols.clear();
        }

        let valid_stems: HashSet<&str> = self
            .entries
            .iter()
            .map(|entry| entry.file_stem.as_str())
            .collect();
        self.image_protocols
            .retain(|stem, _| valid_stems.contains(stem.as_str()));
        self.decoded_images
            .retain(|stem, _| valid_stems.contains(stem.as_str()));
        self.requested_images
            .retain(|stem| valid_stems.contains(stem.as_str()));

        let fs = picker.font_size();
        let font_size = (fs.width, fs.height);

        // icon decode/resize is CPU-bound, like the initial scan. without
        // a limit, a few-hundred-mod pack fires that many spawn_blocking
        // tasks at once, contending with (and starving) the scan that's
        // often still streaming entries in — everything feels sluggish
        // even though each icon is cheap. bound it like scan_mods does.
        let semaphore = self.image_load_semaphore.clone();

        for entry in &self.entries {
            if entry.icon_bytes.is_none() {
                continue;
            }
            if !self.requested_images.insert(entry.file_stem.clone()) {
                continue;
            }
            if let Some(image) = self.decoded_images.get(&entry.file_stem) {
                if rebuilt >= IMAGE_REBUILD_PER_TICK {
                    self.requested_images.remove(&entry.file_stem);
                    self.images_dirty = true;
                    continue;
                }
                rebuilt += 1;
                self.image_protocols.insert(
                    entry.file_stem.clone(),
                    picker.new_resize_protocol(image.clone()),
                );
                continue;
            }
            let file_stem = entry.file_stem.clone();
            let path = entry.path.clone();
            let bytes = entry.icon_bytes.clone().unwrap_or_default();
            let rows = entry.icon_lines.as_ref().map_or(3, Vec::len) as u32;
            let columns = square_icon_columns(rows as u16, font_size);
            let pending = self.pending_images.clone();
            let semaphore = semaphore.clone();

            tokio::spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("image load semaphore is never closed");
                let result = tokio::task::spawn_blocking(move || {
                    let image = image::load_from_memory(&bytes).ok()?;
                    let icon_lines = if use_quadrants {
                        crate::instance::content::mods::make_icon_quadrants_from_image(
                            &image,
                            columns,
                            rows as u16,
                        )
                    } else {
                        crate::instance::content::mods::make_icon_pixels_from_image(
                            &image,
                            columns,
                            rows as u16,
                        )
                    };
                    let side = rows * u32::from(font_size.1.max(1));
                    // Triangle rather than Lanczos3, same reasoning as
                    // web_icon.rs: these render a few cells wide, so the
                    // sharper-but-slower filter buys nothing visible. runs
                    // once per icon per open/scroll, and was the main
                    // source of sluggishness on mod lists with lots of
                    // large (512x512) embedded icons.
                    let image = use_image_protocol.then(|| {
                        image.resize_exact(side, side, image::imageops::FilterType::Triangle)
                    });
                    Some(PendingContentImage {
                        file_stem,
                        path,
                        icon_lines,
                        image,
                    })
                })
                .await
                .ok()
                .flatten();

                if let Some(result) = result
                    && let Ok(mut pending) = pending.lock()
                {
                    pending.push(result);
                    crate::tui::request_redraw();
                }
            });
        }
    }

    pub fn drain_image_loads(&mut self, picker: &ratatui_image::picker::Picker) {
        let images = match self.pending_images.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(_) => return,
        };

        for result in images {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.file_stem == result.file_stem && entry.path == result.path)
            {
                entry.icon_lines = Some(result.icon_lines);
                if let Some(image) = result.image {
                    self.decoded_images
                        .insert(result.file_stem.clone(), image.clone());
                    self.image_protocols
                        .insert(result.file_stem, picker.new_resize_protocol(image));
                }
            } else {
                self.requested_images.remove(&result.file_stem);
                self.images_dirty = true;
            }
        }
    }

    pub fn invalidate_image_protocols(&mut self) {
        self.image_protocols.clear();
        self.requested_images.clear();
        self.sixel_drawn_rects.clear();
        self.images_dirty = true;
    }

    // drain streaming entries from the initial load. each entry arrives
    // individually and is inserted in sorted position for a smooth fill-in
    pub fn drain_pending(&mut self) {
        let Some(rx) = &self.stream_rx else {
            return;
        };

        let mut received = false;
        let mut received_count = 0usize;
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(entry) => {
                    received = true;
                    self.images_dirty = true;
                    received_count += 1;
                    self.display_metadata
                        .insert(entry.file_stem.clone(), display_metadata(&entry));
                    let pos = self
                        .entries
                        .binary_search_by(|e| e.name.to_lowercase().cmp(&entry.name.to_lowercase()))
                        .unwrap_or_else(|i| i);
                    self.entries.insert(pos, entry);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.stream_rx = None;
                    finished = true;
                    break;
                }
            }
        }

        if received || finished {
            self.loading = false;
            if received_count > 0 {
                tracing::trace!(
                    "Drained {} streamed content entries for {}",
                    received_count,
                    self.loaded_for.as_deref().unwrap_or("<unknown>")
                );
            }
            if finished {
                tracing::debug!(
                    "Finished content scan for {} with {} entries",
                    self.loaded_for.as_deref().unwrap_or("<unknown>"),
                    self.entries.len()
                );
            }
            if self.list_state.selected.is_none() && !self.entries.is_empty() {
                self.list_state.selected = Some(0);
            }
            self.update_scrollbar();
        }
    }

    // pick up the precomputed diff from the notify watcher callback.
    // skip while streaming is in progress to avoid duplicate entries.
    pub fn drain_watcher(&mut self) {
        if self.stream_rx.is_some() {
            return;
        }

        let diff = match self.watcher_diff.lock() {
            Ok(mut slot) => slot.take(),
            _ => None,
        };

        let Some(diff) = diff else {
            return;
        };
        self.images_dirty = true;

        // apply toggles (enabled/path changes)
        tracing::debug!(
            "Applying content watcher diff for {}: toggled={} removed={} added={} changed={}",
            self.loaded_for.as_deref().unwrap_or("<unknown>"),
            diff.toggled.len(),
            diff.removed.len(),
            diff.added.len(),
            diff.changed.len()
        );
        for (stem, enabled, path) in &diff.toggled {
            if let Some(entry) = self.entries.iter_mut().find(|e| &e.file_stem == stem) {
                entry.enabled = *enabled;
                entry.path = path.clone();
            }
        }

        // apply in-place content changes (e.g. icon.png/pack.png replaced
        // without renaming the entry itself). replace the stale entry, and
        // evict it from the image caches — request_image_loads gates on
        // requested_images so a stem that was already loaded once would
        // otherwise never be re-fetched even though images_dirty is set.
        for entry in diff.changed {
            self.display_metadata
                .insert(entry.file_stem.clone(), display_metadata(&entry));
            self.requested_images.remove(&entry.file_stem);
            self.image_protocols.remove(&entry.file_stem);
            self.decoded_images.remove(&entry.file_stem);
            if let Some(slot) = self
                .entries
                .iter_mut()
                .find(|e| e.file_stem == entry.file_stem)
            {
                *slot = entry;
            }
        }

        // apply removals
        if !diff.removed.is_empty() {
            self.entries
                .retain(|e| !diff.removed.contains(&e.file_stem));
            for stem in &diff.removed {
                self.display_metadata.remove(stem);
            }
            // files deleted outside an install (user's file manager, another
            // launcher, rm) may be ones the installed-content sidecar tracks
            // for a catalog project — clear those records so the browse
            // popup doesn't keep an "installed" badge for a ghost file.
            if let Some(dir) = self.watched_dir.as_deref() {
                let stems: Vec<String> = diff
                    .removed
                    .iter()
                    .filter(|stem| {
                        // only entries actually gone from disk; a "removal"
                        // that's really a rename (enable/disable toggle)
                        // reappears with the other spelling in the same diff
                        !dir.join(format!("{stem}{}", self.content_ext.unwrap_or(".jar")))
                            .exists()
                            && !dir
                                .join(format!(
                                    "{stem}{}.disabled",
                                    self.content_ext.unwrap_or(".jar")
                                ))
                                .exists()
                    })
                    .cloned()
                    .collect();
                if !stems.is_empty() {
                    crate::instance::content::installed_meta::remove_stems(dir, &stems);
                    crate::tui::widgets::popups::content_browse::refresh_installed(dir);
                }
            }
        }

        // insert new entries in sorted position
        for entry in diff.added {
            self.display_metadata
                .insert(entry.file_stem.clone(), display_metadata(&entry));
            let pos = self
                .entries
                .binary_search_by(|e| e.name.to_lowercase().cmp(&entry.name.to_lowercase()))
                .unwrap_or_else(|i| i);
            self.entries.insert(pos, entry);
        }

        // clamp selected
        if let Some(sel) = self.list_state.selected {
            if self.entries.is_empty() {
                self.list_state.selected = None;
            } else {
                self.list_state.selected = Some(sel.min(self.entries.len().saturating_sub(1)));
            }
        }

        self.update_scrollbar();
    }

    // starts a notify file watcher on the given directory. changes trigger
    // a background diff that lands in watcher_diff for drain_watcher to apply.
    pub fn watch_dir(&mut self, dir: std::path::PathBuf) {
        use notify::{RecursiveMode, Watcher};
        use std::sync::atomic::{AtomicBool, Ordering};

        // drop previous watcher
        self._watcher = None;

        // fresh slot for this watcher, not a clone of the old one. a
        // dropped watcher can still have an event in flight on its
        // background thread (e.g. the REMOVE fs event that an instance
        // rename fires for the old directory) - if it shared our slot,
        // that stale diff could land here after we've already switched
        // instances and clobber the new instance's just-scanned entries
        // (removed-by-stem matches are especially likely to hit, since a
        // renamed instance keeps the exact same mod/resourcepack files).
        // giving every watcher its own Arc means a late write from the
        // old one lands in an orphaned slot nobody reads anymore.
        self.watcher_diff = Arc::new(Mutex::new(None));
        let watcher_diff = self.watcher_diff.clone();
        let ext: &'static str = self.content_ext.unwrap_or(".jar");
        let scan_one = self.scan_one_fn;

        let dirty = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(false));
        let dirty_cb = dirty.clone();
        let running_cb = running.clone();

        // initialize known stems from the current directory state so existing
        // files are not treated as "new" on the first notify event
        let known_stems = Arc::new(Mutex::new(read_dir_stems(&dir, ext)));

        let watch_dir = dir.clone();
        let watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            if let Err(e) = &res {
                tracing::warn!(
                    "Content watcher event error for {}: {}",
                    watch_dir.display(),
                    e
                );
                return;
            }

            // mark dirty. if a thread is already running it will loop to
            // pick up the change after its current diff
            dirty_cb.store(true, Ordering::Relaxed);

            if running_cb.swap(true, Ordering::Relaxed) {
                return;
            }

            let dir = watch_dir.clone();
            let diff_slot = watcher_diff.clone();
            let dirty = dirty_cb.clone();
            let running = running_cb.clone();
            let known = known_stems.clone();

            std::thread::spawn(move || {
                // always clear `running` even if we panic
                struct ResetOnDrop(Arc<AtomicBool>);
                impl Drop for ResetOnDrop {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Relaxed);
                    }
                }
                let _guard = ResetOnDrop(running);

                loop {
                    dirty.store(false, Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(100));

                    let result = (|| {
                        let on_disk = read_dir_stems(&dir, ext);
                        let mut known_map = known.lock().ok()?;

                        let mut toggled = Vec::new();
                        let mut removed = Vec::new();
                        let mut added = Vec::new();
                        let mut changed = Vec::new();

                        for (stem, (old_path, old_enabled, old_sig)) in known_map.iter() {
                            if let Some((disk_path, disk_enabled, disk_sig)) = on_disk.get(stem) {
                                if *disk_enabled != *old_enabled || *disk_path != *old_path {
                                    toggled.push((stem.clone(), *disk_enabled, disk_path.clone()));
                                } else if disk_sig != old_sig
                                    && let Some(scan_one) = scan_one
                                {
                                    changed.push(scan_one(disk_path, stem, *disk_enabled));
                                }
                            } else {
                                removed.push(stem.clone());
                            }
                        }

                        for (stem, (path, enabled, _sig)) in &on_disk {
                            if !known_map.contains_key(stem)
                                && let Some(scan_one) = scan_one
                            {
                                added.push(scan_one(path, stem, *enabled));
                            }
                        }

                        *known_map = on_disk;

                        if toggled.is_empty()
                            && removed.is_empty()
                            && added.is_empty()
                            && changed.is_empty()
                        {
                            None
                        } else {
                            Some(WatcherDiff {
                                toggled,
                                removed,
                                added,
                                changed,
                            })
                        }
                    })();

                    if let Some(diff) = result
                        && let Ok(mut slot) = diff_slot.lock()
                    {
                        *slot = Some(diff);
                        crate::tui::request_redraw();
                    }

                    if !dirty.load(Ordering::Relaxed) {
                        break;
                    }
                }
            });
        });

        match watcher {
            Ok(mut w) => {
                if let Err(e) = w.watch(&dir, RecursiveMode::Recursive) {
                    tracing::warn!("Failed to watch {}: {e}", dir.display());
                } else {
                    tracing::debug!("Watching content directory {}", dir.display());
                    self._watcher = Some(w);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to create file watcher: {e}");
            }
        }

        self.watched_dir = Some(dir);
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.search.matches(&e.name))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn pending_delete(&self) -> Option<PendingContentDelete> {
        let filtered = self.filtered_indices();
        let real_idx = self.list_state.selected.and_then(|i| filtered.get(i))?;
        let entry = self.entries.get(*real_idx)?;
        Some(PendingContentDelete {
            name: entry.name.clone(),
            path: entry.path.clone(),
        })
    }
}

impl ContentListState {
    // saves current entries to cache before loading new ones, and restores
    // from cache if this instance was seen before (avoids re-scanning).
    // content_dir is the actual directory to scan (e.g. .minecraft/mods).
    pub fn start_load(
        &mut self,
        content_dir: &Path,
        instance_name: &str,
        scan_one_fn: ScanOneFn,
        ext: &'static str,
    ) {
        self.scan_one_fn = Some(scan_one_fn);
        self.content_ext = Some(ext);
        self.images_dirty = true;

        // save current entries to cache
        if let Some(prev) = self.loaded_for.take()
            && !self.entries.is_empty()
        {
            tracing::trace!(
                "Caching {} content entries for {}",
                self.entries.len(),
                prev
            );
            self.cache.insert(
                prev,
                CachedList {
                    entries: std::mem::take(&mut self.entries),
                    selected: self.list_state.selected,
                    // take the decoded protocols along so the next visit to
                    // this instance can restore them instead of re-decoding.
                    image_protocols: std::mem::take(&mut self.image_protocols),
                    decoded_images: std::mem::take(&mut self.decoded_images),
                },
            );
        }
        // anything that was still decoding when we switched away can't be
        // re-targeted once its task lands (drain_image_loads matches by
        // stem+path), so drop the bookkeeping. also drop the stale Sixel
        // rects from the previous instance's grid - without this, its
        // leftover cells would be cleared (or skipped) against the new
        // instance's rows, which sit at different coordinates.
        self.image_protocols.clear();
        self.requested_images.clear();
        self.sixel_drawn_rects.clear();
        self.decoded_images.clear();

        // try cache first
        if let Some(cached) = self.cache.remove(instance_name) {
            self.entries = cached.entries;
            self.rebuild_display_metadata();
            self.list_state.selected = cached.selected;
            // put the decoded protocols back and mark their stems already
            // requested so request_image_loads skips re-decoding them;
            // images_dirty stays true so stems that changed while we were
            // away (or weren't in the cache) still get decoded.
            self.image_protocols = cached.image_protocols;
            self.decoded_images = cached.decoded_images;
            self.requested_images = self.image_protocols.keys().cloned().collect();
            self.loading = false;
            self.stream_rx = None;
            self.loaded_for = Some(instance_name.to_string());
            self.update_scrollbar();
            tracing::debug!(
                "Restored {} cached content entries for {}",
                self.entries.len(),
                instance_name
            );
            return;
        }

        // no cache, stream entries one by one as each file is scanned
        self.entries.clear();
        self.display_metadata.clear();
        self.list_state = TuiListState::default();
        self.loading = true;
        self.loaded_for = Some(instance_name.to_string());
        self.update_scrollbar();

        let (tx, rx) = mpsc::channel();
        self.stream_rx = Some(rx);

        let dir = content_dir.to_path_buf();
        tracing::debug!(
            "Starting content scan for {} in {}",
            instance_name,
            content_dir.display()
        );

        tokio::spawn(async move {
            // Phase 1: list the directory and classify names. This is just
            // filesystem metadata, not zip I/O, so it stays cheap and serial.
            let items = tokio::task::spawn_blocking(move || {
                let read_dir = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(e) => {
                        tracing::warn!("Failed to read content directory {}: {}", dir.display(), e);
                        return Vec::new();
                    }
                };
                let disabled_ext = format!("{ext}.disabled");
                let mut items = Vec::new();

                for dir_entry in read_dir.flatten() {
                    let path = dir_entry.path();
                    let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                        tracing::trace!(
                            "Skipping content path with invalid filename: {}",
                            path.display()
                        );
                        continue;
                    };

                    let (enabled, file_stem) = if let Some(stem) = fname.strip_suffix(&disabled_ext)
                    {
                        (false, stem.to_owned())
                    } else if let Some(stem) = fname.strip_suffix(ext) {
                        (true, stem.to_owned())
                    } else if path.is_dir() {
                        crate::instance::content::parse_enabled_stem_dir(fname)
                    } else {
                        tracing::trace!(
                            "Skipping content path with unsupported extension: {}",
                            path.display()
                        );
                        continue;
                    };

                    items.push((path, file_stem, enabled));
                }
                items
            })
            .await
            .unwrap_or_default();

            // Phase 2: the actual per-item work (open the zip, parse loader
            // metadata, decode/resize the fallback icon) is independent per
            // file and was previously done one item at a time on a single
            // thread — for a modpack with a few hundred jars that serialized
            // a lot of zip I/O and PNG decoding that could run concurrently.
            // Fan it out across a bounded pool instead; entries still stream
            // back as each one finishes.
            let concurrency = std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(4);
            let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
            let mut join_set = tokio::task::JoinSet::new();

            for (path, file_stem, enabled) in items {
                let semaphore = semaphore.clone();
                let tx = tx.clone();
                join_set.spawn(async move {
                    let Ok(_permit) = semaphore.acquire().await else {
                        return false;
                    };
                    match tokio::task::spawn_blocking(move || scan_one_fn(&path, &file_stem, enabled))
                        .await
                    {
                        Ok(entry) => {
                            if tx.send(entry).is_err() {
                                return false; // receiver dropped (instance switched)
                            }
                            crate::tui::request_redraw();
                            true
                        }
                        Err(_) => true,
                    }
                });
            }

            while let Some(res) = join_set.join_next().await {
                if matches!(res, Ok(false)) {
                    join_set.abort_all();
                    break;
                }
            }
        });
    }

    fn update_scrollbar(&mut self) {
        let count = self.entries.len();
        let max = count.saturating_sub(1);
        let pos = self.list_state.selected.unwrap_or(0);
        self.scrollbar_state = ScrollbarState::new(max).position(pos);
    }

    fn rebuild_display_metadata(&mut self) {
        self.display_metadata = self
            .entries
            .iter()
            .map(|entry| (entry.file_stem.clone(), display_metadata(entry)))
            .collect();
    }

    // enable/disable by renaming the file with/without .disabled extension.
    // this is how most minecraft launchers handle it
    pub fn toggle_selected(&mut self) {
        let Some(index) = self.list_state.selected else {
            return;
        };
        let Some(entry) = self.entries.get(index) else {
            return;
        };

        let new_path = if entry.enabled {
            let fname = match entry.path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => return,
            };
            let mut p = entry.path.clone();
            p.set_file_name(format!("{fname}.disabled"));
            p
        } else {
            let fname = match entry.path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => return,
            };
            let mut p = entry.path.clone();
            p.set_file_name(fname.trim_end_matches(".disabled"));
            p
        };

        match std::fs::rename(&entry.path, &new_path) {
            Ok(()) => {
                let entry = &mut self.entries[index];
                entry.enabled = !entry.enabled;
                entry.path = new_path;
            }
            Err(e) => {
                tracing::error!(
                    "Failed to toggle '{}' from {} to {}: {}",
                    entry.file_stem,
                    entry.path.display(),
                    new_path.display(),
                    e
                );
            }
        }
    }

    pub fn remove_path(&mut self, path: &Path) {
        let file_stem = self
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.file_stem.clone());
        self.entries.retain(|entry| entry.path != path);
        if let Some(file_stem) = file_stem {
            self.image_protocols.remove(&file_stem);
            self.decoded_images.remove(&file_stem);
            self.requested_images.remove(&file_stem);
            self.display_metadata.remove(&file_stem);
        }
        self.images_dirty = true;
        if let Some(sel) = self.list_state.selected {
            let visible_count = self.filtered_indices().len();
            if visible_count == 0 {
                self.list_state.selected = None;
            } else {
                self.list_state.selected = Some(sel.min(visible_count.saturating_sub(1)));
            }
        }
        self.update_scrollbar();
    }
}

// routes search-mode keys through SearchState::handle_key and applies the
// one side effect every list search shares: when the query changes, jump
// the selection back to the top of the filtered results.
fn handle_search_keys(key_event: &KeyEvent, state: &mut ContentListState) -> bool {
    use crate::tui::widgets::search::SearchAction;
    match state.search.handle_key(key_event) {
        SearchAction::Unhandled => false,
        SearchAction::Activated
        | SearchAction::Edited
        | SearchAction::Confirmed
        | SearchAction::Deactivated => {
            state.list_state.selected = Some(0);
            state.update_scrollbar();
            true
        }
        SearchAction::Handled => true,
    }
}

// ctrl+o: open the selected entry's containing folder, or - when there's
// no selection (list empty, or the content dir doesn't exist yet, e.g. a
// mods folder that's never been populated) - fall back to the tab's own
// content dir, creating it first so there's actually something to open
// instead of silently doing nothing.
fn open_selected_or_watched_dir(state: &ContentListState, filtered: &[usize]) {
    let dir = state
        .list_state
        .selected
        .and_then(|i| filtered.get(i))
        .and_then(|&real_idx| state.entries[real_idx].path.parent())
        .map(|p| p.to_path_buf())
        .or_else(|| state.watched_dir.clone());

    let Some(dir) = dir else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::error!("Failed to create directory {}: {}", dir.display(), e);
        return;
    }
    if let Err(e) = open::that_detached(&dir) {
        tracing::error!("Failed to open directory: {}", e);
    }
}

pub fn handle_key_no_toggle(key_event: &KeyEvent, state: &mut ContentListState) -> bool {
    if handle_search_keys(key_event, state) {
        return true;
    }
    let filtered = state.filtered_indices();
    let count = filtered.len();

    match key_event.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if count == 0 {
                return true;
            }
            let current = state.list_state.selected.unwrap_or(0);
            state.list_state.selected = Some((current + 1).min(count - 1));
            state.update_scrollbar();
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let current = state.list_state.selected.unwrap_or(0);
            state.list_state.selected = Some(current.saturating_sub(1));
            state.update_scrollbar();
            true
        }
        // ctrl+o opens the containing folder. (was shift+enter, but most
        // terminals can't report shift+enter distinctly from plain enter
        // without kitty keyboard protocol support, so ctrl+o is used instead
        // since it works everywhere)
        KeyCode::Char('o') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
            open_selected_or_watched_dir(state, &filtered);
            true
        }
        _ => false,
    }
}

pub fn handle_key(key_event: &KeyEvent, state: &mut ContentListState) -> bool {
    if handle_search_keys(key_event, state) {
        return true;
    }
    let filtered = state.filtered_indices();
    let count = filtered.len();

    match key_event.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if count == 0 {
                return true;
            }
            let current = state.list_state.selected.unwrap_or(0);
            state.list_state.selected = Some((current + 1).min(count - 1));
            state.update_scrollbar();
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let current = state.list_state.selected.unwrap_or(0);
            state.list_state.selected = Some(current.saturating_sub(1));
            state.update_scrollbar();
            true
        }
        // ctrl+o opens the containing folder. (was shift+enter, but most
        // terminals can't report shift+enter distinctly from plain enter
        // without kitty keyboard protocol support, so ctrl+o is used instead
        // since it works everywhere)
        KeyCode::Char('o') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
            open_selected_or_watched_dir(state, &filtered);
            true
        }
        KeyCode::Enter => {
            if let Some(&real_idx) = state.list_state.selected.and_then(|i| filtered.get(i)) {
                state.list_state.selected = Some(real_idx);
                state.toggle_selected();
                state.list_state.selected =
                    Some(filtered.iter().position(|&i| i == real_idx).unwrap_or(0));
            }
            true
        }
        _ => false,
    }
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &mut ContentListState,
    is_focused: bool,
    loading_text: &str,
    empty_text: &str,
    picker: &ratatui_image::picker::Picker,
) {
    let theme = THEME.as_ref();
    if state.loading {
        frame.render_widget(
            Paragraph::new(loading_text).style(Style::default().fg(theme.text_dim())),
            area,
        );
        return;
    }

    let filtered = state.filtered_indices();

    if filtered.is_empty() {
        state.list_state.selected = None;
        frame.render_widget(
            Paragraph::new(empty_text).style(Style::default().fg(theme.text_dim())),
            area,
        );
        return;
    }

    let count = filtered.len();

    // clamp selected so the ListView builder never gets an out-of-bounds index
    if let Some(sel) = state.list_state.selected
        && sel >= count
    {
        state.list_state.selected = Some(count.saturating_sub(1));
    }

    let use_image_protocol =
        picker.protocol_type() != ratatui_image::picker::ProtocolType::Halfblocks;
    let entries = &state.entries;
    let filtered_rows = &filtered;
    let list_width = area.width as usize;

    let display_metadata = &state.display_metadata;
    let builder = ListBuilder::new(move |context| {
        let theme = THEME.as_ref();
        let entry = &entries[filtered_rows[context.index]];
        let name = &entry.name;
        let metadata = display_metadata.get(&entry.file_stem);
        let enabled = entry.enabled;
        let icon_pixels = &entry.icon_lines;
        let has_image = entry.icon_bytes.is_some();
        let protocol_columns = protocol_icon_columns(entry, picker) as usize;
        let show_selected = is_focused && context.is_selected;
        let use_mc_colors = enabled;

        let stripe_bg = if context.index % 2 == 0 {
            theme.background()
        } else {
            theme.stripe()
        };

        let (name_style, description_style, background) = match (enabled, show_selected) {
            (true, true) => (
                Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.text_dim()),
                stripe_bg,
            ),
            (true, false) => (
                Style::default()
                    .fg(theme.text())
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.text_dim()),
                stripe_bg,
            ),
            (false, true) => (
                Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::CROSSED_OUT),
                Style::default().fg(theme.text_dim()),
                stripe_bg,
            ),
            (false, false) => (
                Style::default()
                    .fg(theme.text_dim())
                    .add_modifier(Modifier::CROSSED_OUT),
                Style::default().fg(theme.text_dim()),
                stripe_bg,
            ),
        };

        let has_icon = icon_pixels.is_some();
        let stripped_desc = metadata.map_or("", |metadata| metadata.description.as_str());
        let has_description = metadata.is_some_and(|metadata| metadata.has_description);
        let compact = !has_icon && !has_description;

        let selector = if show_selected {
            Span::styled("\u{258c}", Style::default().fg(theme.accent()))
        } else {
            Span::raw(" ")
        };

        // Calculate available width for the name.
        let name_width = list_width.saturating_sub(1 + if has_icon { protocol_columns + 1 } else { 0 });

        if compact {
            let mut line = Vec::new();
            line.push(selector.clone());
            if use_mc_colors {
                line.extend(parse_mc_text(&truncate_str(name, name_width), name_style));
            } else {
                line.push(Span::styled(truncate_str(&strip_mc_codes(name), name_width), name_style));
            }

            let item = Text::from(vec![Line::from(line)]).style(Style::default().bg(background));
            (item, 1)
        } else if has_icon {
            let text_rows = if has_description { 2 } else { 1 }; // name + optional description
            // icons are generated at a fixed height (see scan_one_mod), which can be
            // taller than the text area next to them. cap what we show to text_rows so
            // there's never an orphan icon row with nothing beside it, bleeding into
            // the gap before the next entry.
            let icon_row_count = icon_pixels
                .as_ref()
                .map(|r| r.len())
                .unwrap_or(0)
                .min(text_rows);
            let height = text_rows as u16;

            let pad = if show_selected {
                Span::styled("\u{258c}", Style::default().fg(theme.accent()))
            } else {
                Span::raw(" ")
            };

            let mut line_0 = vec![selector.clone()];
            line_0.extend(icon_spans(
                icon_pixels.as_ref(),
                0,
                use_image_protocol && has_image,
                protocol_columns,
            ));
            line_0.push(Span::raw(" "));
            if use_mc_colors {
                line_0.extend(parse_mc_text(&truncate_str(name, name_width), name_style));
            } else {
                line_0.push(Span::styled(truncate_str(&strip_mc_codes(name), name_width), name_style));
            }

            let mut lines = vec![Line::from(line_0)];

            if has_description {
                let mut row = vec![pad.clone()];
                row.extend(icon_spans(
                    icon_pixels.as_ref(),
                    1,
                    use_image_protocol && has_image,
                    protocol_columns,
                ));
                row.push(Span::raw(" "));
                let text_width = list_width.saturating_sub(2 + protocol_columns + 1);
                row.push(Span::styled(
                    truncate_str(stripped_desc, text_width),
                    description_style,
                ));
                lines.push(Line::from(row));
            }

            let desc_rows = if has_description { 1 } else { 0 };
            for r in (1 + desc_rows)..icon_row_count {
                let mut row = vec![pad.clone()];
                row.extend(icon_spans(
                    icon_pixels.as_ref(),
                    r,
                    use_image_protocol && has_image,
                    protocol_columns,
                ));
                lines.push(Line::from(row));
            }

            let item = Text::from(lines).style(Style::default().bg(background));
            (item, height)
        } else {
            let mut line_0 = Vec::new();
            line_0.push(selector.clone());
            if use_mc_colors {
                line_0.extend(parse_mc_text(&truncate_str(name, name_width), name_style));
            } else {
                line_0.push(Span::styled(truncate_str(&strip_mc_codes(name), name_width), name_style));
            }

            let mut lines = vec![Line::from(line_0)];

            if has_description {
                let pad = if show_selected {
                    Span::styled("\u{258c}", Style::default().fg(theme.accent()))
                } else {
                    Span::raw(" ")
                };
                let text_width = list_width.saturating_sub(2);
                lines.push(Line::from(vec![
                    pad,
                    Span::styled(
                        truncate_str(stripped_desc, text_width),
                        description_style,
                    ),
                ]));
            }

            let height = lines.len() as u16;
            let item = Text::from(lines).style(Style::default().bg(background));
            (item, height)
        }
    });

    let list = ListView::new(builder, count);
    frame.render_stateful_widget(list, area, &mut state.list_state);

    if picker.protocol_type() != ratatui_image::picker::ProtocolType::Halfblocks {
        render_image_icons(frame, area, state, &filtered, picker);
    }

    let scrollbar_area = Rect {
        x: area.x + area.width.saturating_sub(0),
        y: area.y + 1,
        width: 1,
        height: area.height.saturating_sub(2),
    };
    frame.render_stateful_widget(
        Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{25b2}"))
            .style(
                Style::default()
                    .fg(theme.text_dim())
                    .add_modifier(Modifier::BOLD),
            )
            .thumb_symbol("\u{2551}")
            .track_symbol(Some(""))
            .end_symbol(Some("\u{25bc}")),
        scrollbar_area,
        &mut state.scrollbar_state,
    );
}

fn render_image_icons(
    frame: &mut Frame,
    area: Rect,
    state: &mut ContentListState,
    filtered: &[usize],
    picker: &ratatui_image::picker::Picker,
) {
    let truncation = state.list_state.scroll_truncation();
    let mut y = area.y;
    let first = state.list_state.scroll_offset_index();

    // Sixel has no addressable image objects or delete command, so stale
    // pixels only vanish once something erases that cell. clearing every
    // icon every frame (even unchanged ones) caused flicker: most terminals
    // don't rasterize Sixel instantly, so blank-then-repaint is a visible
    // strobe on larger icons. instead, only clear a row's cells when its
    // rect differs from last frame (moved, resized, or icon gone).
    let is_sixel = picker.protocol_type() == ratatui_image::picker::ProtocolType::Sixel;
    let mut new_sixel_rects: HashMap<String, Rect> = HashMap::new();

    for (visible_index, &entry_index) in filtered.iter().enumerate().skip(first) {
        let Some(entry) = state.entries.get(entry_index) else {
            continue;
        };
        let icon_rows = entry.icon_lines.as_ref().map_or(0, Vec::len);
        let has_description = state
            .display_metadata
            .get(&entry.file_stem)
            .is_some_and(|metadata| metadata.has_description);
        let text_rows = if has_description { 2 } else { 1 };
        // match the cap applied in the text-mode builder: never draw the image
        // taller than the name/description area beside it.
        let icon_rows = icon_rows.min(text_rows);
        let height = text_rows as u16;

        if y >= area.y + area.height {
            break;
        }
        let top_crop = if visible_index == first {
            truncation.min(icon_rows as u16)
        } else {
            0
        };
        let visible_icon_rows = (icon_rows as u16)
            .saturating_sub(top_crop)
            .min(area.y + area.height - y);
        if visible_icon_rows > 0
            && entry.icon_bytes.is_some()
            && let Some(protocol) = state.image_protocols.get_mut(&entry.file_stem)
        {
            let icon_area = Rect {
                x: area.x + 1,
                y,
                width: protocol_icon_columns(entry, picker).min(area.width.saturating_sub(1)),
                height: visible_icon_rows,
            };
            if icon_area.height > 0 && icon_area.width > 0 {
                if is_sixel {
                    new_sixel_rects.insert(entry.file_stem.clone(), icon_area);
                    let changed = state.sixel_drawn_rects.get(&entry.file_stem) != Some(&icon_area);
                    if changed {
                        frame.render_widget(Clear, icon_area);
                    }
                }
                let clipped = top_crop > 0 || visible_icon_rows < icon_rows as u16;
                let resize = if clipped {
                    Resize::Crop(Some(CropOptions {
                        clip_top: top_crop > 0,
                        clip_left: false,
                    }))
                } else {
                    Resize::Scale(None)
                };
                let widget: StatefulImage<StatefulProtocol> =
                    StatefulImage::default().resize(resize);
                frame.render_stateful_widget(widget, icon_area, protocol);
            }
        }
        let visible_height = if visible_index == first {
            height.saturating_sub(truncation)
        } else {
            height
        };
        y = y.saturating_add(visible_height);
        if visible_index + 1 >= filtered.len() {
            break;
        }
    }

    if is_sixel {
        // Anything drawn last frame but not repainted this frame (scrolled
        // out, icon finished loading and is now smaller/absent, entry no
        // longer filtered in) still has stale pixels on screen — clear those
        // leftover rects explicitly since nothing else will touch them.
        for (stem, old_rect) in &state.sixel_drawn_rects {
            if new_sixel_rects.get(stem) != Some(old_rect) {
                frame.render_widget(Clear, *old_rect);
            }
        }
        state.sixel_drawn_rects = new_sixel_rects;
    }
}

// minecraft's 16-color palette, keyed by the formatting code character.
// these exact RGB values come from the minecraft wiki
fn mc_color(code: char) -> Option<Color> {
    match code {
        '0' => Some(Color::Rgb(0x00, 0x00, 0x00)),
        '1' => Some(Color::Rgb(0x00, 0x00, 0xAA)),
        '2' => Some(Color::Rgb(0x00, 0xAA, 0x00)),
        '3' => Some(Color::Rgb(0x00, 0xAA, 0xAA)),
        '4' => Some(Color::Rgb(0xAA, 0x00, 0x00)),
        '5' => Some(Color::Rgb(0xAA, 0x00, 0xAA)),
        '6' => Some(Color::Rgb(0xFF, 0xAA, 0x00)),
        '7' => Some(Color::Rgb(0xAA, 0xAA, 0xAA)),
        '8' => Some(Color::Rgb(0x55, 0x55, 0x55)),
        '9' => Some(Color::Rgb(0x55, 0x55, 0xFF)),
        'a' | 'A' => Some(Color::Rgb(0x55, 0xFF, 0x55)),
        'b' | 'B' => Some(Color::Rgb(0x55, 0xFF, 0xFF)),
        'c' | 'C' => Some(Color::Rgb(0xFF, 0x55, 0x55)),
        'd' | 'D' => Some(Color::Rgb(0xFF, 0x55, 0xFF)),
        'e' | 'E' => Some(Color::Rgb(0xFF, 0xFF, 0x55)),
        'f' | 'F' => Some(Color::Rgb(0xFF, 0xFF, 0xFF)),
        _ => None,
    }
}

// parses minecraft's section-sign (U+00A7) formatting codes into styled spans.
// handles colors (0-f), bold (l), strikethrough (m), underline (n), italic (o), reset (r)
fn parse_mc_text(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current_style = base_style;
    let mut current_text = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{00A7}'
            && let Some(&code) = chars.peek()
        {
            if !current_text.is_empty() {
                spans.push(Span::styled(current_text.clone(), current_style));
                current_text.clear();
            }
            chars.next();

            if let Some(color) = mc_color(code) {
                current_style = base_style.fg(color);
            } else {
                match code {
                    'l' | 'L' => {
                        current_style = current_style.add_modifier(Modifier::BOLD);
                    }
                    'm' | 'M' => {
                        current_style = current_style.add_modifier(Modifier::CROSSED_OUT);
                    }
                    'n' | 'N' => {
                        current_style = current_style.add_modifier(Modifier::UNDERLINED);
                    }
                    'o' | 'O' => {
                        current_style = current_style.add_modifier(Modifier::ITALIC);
                    }
                    'r' | 'R' => {
                        current_style = base_style;
                    }
                    _ => {}
                }
            }
            continue;
        }
        current_text.push(ch);
    }

    if !current_text.is_empty() {
        spans.push(Span::styled(current_text, current_style));
    }

    spans
}

fn strip_mc_codes(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{00A7}' {
            chars.next();
        } else {
            result.push(ch);
        }
    }
    result
}

fn display_metadata(entry: &ContentEntry) -> DisplayMetadata {
    let description = strip_mc_codes(&entry.description);
    let description = description.lines().next().unwrap_or("").trim().to_string();
    DisplayMetadata {
        has_description: !description.is_empty(),
        description,
    }
}

/// Truncate a string to `max_chars` visible characters, appending `…` if truncated.
fn truncate_str(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        text.to_string()
    } else if max_chars <= 1 {
        "\u{2026}".to_string()
    } else {
        let truncated: String = text.chars().take(max_chars - 1).collect();
        format!("{truncated}\u{2026}")
    }
}


// renders one row of a mod icon using half-block characters (U+2584).
// each cell packs two vertical pixels via fg/bg colors, giving
// double the vertical resolution out of the terminal
fn icon_spans(
    icon_pixels: Option<&Vec<Vec<IconCell>>>,
    row: usize,
    use_image_protocol: bool,
    protocol_columns: usize,
) -> Vec<Span<'static>> {
    if use_image_protocol {
        return vec![Span::raw(" ".repeat(protocol_columns))];
    }
    match icon_pixels.and_then(|rows| rows.get(row)) {
        Some(cols) => cols
            .iter()
            .map(|cell| {
                Span::styled(
                    cell.symbol.to_string(),
                    Style::default()
                        .fg(Color::Rgb(cell.fg_r, cell.fg_g, cell.fg_b))
                        .bg(Color::Rgb(cell.bg_r, cell.bg_g, cell.bg_b)),
                )
            })
            .collect(),
        None => {
            let theme = THEME.as_ref();
            vec![Span::styled(
                "      ",
                Style::default().fg(theme.text_dim()),
            )]
        }
    }
}

fn protocol_icon_columns(
    entry: &crate::instance::content::mods::ContentEntry,
    picker: &ratatui_image::picker::Picker,
) -> u16 {
    let rows = entry.icon_lines.as_ref().map_or(3, Vec::len) as u16;
    let fs = picker.font_size();
    square_icon_columns(rows, (fs.width, fs.height))
}

fn square_icon_columns(rows: u16, font_size: (u16, u16)) -> u16 {
    let width = u32::from(font_size.0.max(1));
    let height = u32::from(font_size.1.max(1));
    ((u32::from(rows) * height + width / 2) / width).max(1) as u16
}

// best-effort "last changed" signature for a content path. a plain file
// (mod jar, zipped pack) is just its mtime. a directory (worlds are always
// dirs; packs can be too) also looks one level down, so replacing
// icon.png/pack.png inside it — without touching the dir entry — still
// changes the signature. dir mtimes alone don't catch that: on most
// filesystems they only change when entries are added/removed, not when a
// child's contents are rewritten in place.
fn content_signature(path: &std::path::Path) -> std::time::SystemTime {
    let mut newest = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH);

    if path.is_dir()
        && let Ok(read_dir) = std::fs::read_dir(path)
    {
        for child in read_dir.flatten() {
            if let Ok(modified) = child.metadata().and_then(|m| m.modified())
                && modified > newest
            {
                newest = modified;
            }
        }
    }

    newest
}

// reads a content directory and builds a stem -> (path, enabled, signature)
// map. used both by watch_dir to initialize known state and by the watcher
// thread to detect changes. when ext is empty (worlds), only directories
// are included.
type StemMap = HashMap<String, (std::path::PathBuf, bool, std::time::SystemTime)>;

fn read_dir_stems(dir: &std::path::Path, ext: &str) -> StemMap {
    let mut map = HashMap::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return map;
    };
    let dirs_only = ext.is_empty();
    let disabled_ext = format!("{ext}.disabled");

    for dir_entry in read_dir.flatten() {
        let path = dir_entry.path();
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if dirs_only {
            if !path.is_dir() && !fname.ends_with(".disabled") {
                continue;
            }
            let (enabled, stem) = crate::instance::content::parse_enabled_stem_dir(fname);
            let sig = content_signature(&path);
            map.insert(stem, (path, enabled, sig));
            continue;
        }
        if let Some(stem) = fname.strip_suffix(&disabled_ext) {
            let sig = content_signature(&path);
            map.insert(stem.to_owned(), (path, false, sig));
        } else if let Some(stem) = fname.strip_suffix(ext) {
            let sig = content_signature(&path);
            map.insert(stem.to_owned(), (path, true, sig));
        } else if path.is_dir() {
            let (enabled, stem) = crate::instance::content::parse_enabled_stem_dir(fname);
            let sig = content_signature(&path);
            map.insert(stem, (path, enabled, sig));
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::square_icon_columns;

    #[test]
    fn square_columns_follow_terminal_cell_ratio() {
        assert_eq!(square_icon_columns(3, (8, 16)), 6);
        assert_eq!(square_icon_columns(3, (8, 18)), 7);
        assert_eq!(square_icon_columns(6, (8, 18)), 14);
    }

    #[test]
    fn square_columns_handle_missing_cell_size() {
        assert_eq!(square_icon_columns(3, (0, 0)), 3);
    }
}
