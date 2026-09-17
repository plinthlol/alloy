// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// state machine for the "browse & install" popup reachable from the Mods
// and Resource Packs content tabs. deliberately mirrors the modpack
// browser in new_instance/state.rs (same two-step Browse → Version shape,
// same types, reused rather than duplicated), but installs a single file
// into an *existing* instance's content dir instead of creating a new
// instance from a pack — so there's no name-entry confirm step: picking a
// version installs it immediately.

use crate::instance::models::ModLoader;
use crate::net::curseforge;
use crate::tui::widgets::popups::description;
pub(crate) use crate::tui::widgets::popups::new_instance::{
    LoadState, ModpackHit, ModpackSource, ModpackVersionHit,
};
use crossterm::event::{KeyCode, KeyEvent};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};
use tui_prompts::{State as PromptState, TextState};

pub(crate) static BROWSE_STATE: LazyLock<Arc<Mutex<ContentBrowseState>>> =
    LazyLock::new(|| Arc::new(Mutex::new(ContentBrowseState::default())));

// which content dir/catalog filters this popup instance is browsing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Mod,
    ResourcePack,
}

impl ContentKind {
    pub fn label(self) -> &'static str {
        match self {
            ContentKind::Mod => "Mod",
            ContentKind::ResourcePack => "Resource Pack",
        }
    }

    fn modrinth_project_type(self) -> &'static str {
        match self {
            ContentKind::Mod => "mod",
            ContentKind::ResourcePack => "resourcepack",
        }
    }

    fn curseforge_class_id(self) -> u32 {
        match self {
            ContentKind::Mod => curseforge::CLASS_ID_MOD,
            ContentKind::ResourcePack => curseforge::CLASS_ID_RESOURCE_PACK,
        }
    }
}

// PageUp/PageDown jump size for the search-results and version lists.
const PAGE_STEP: usize = 10;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BrowseStep {
    #[default]
    Search,
    Version,
}

#[derive(Debug, Clone)]
pub enum ContentInstallSource {
    Modrinth(crate::net::modrinth::ProjectVersion),
    CurseForge { file: curseforge::ModFile },
}

#[derive(Debug, Clone)]
pub struct ContentInstallParams {
    pub dest_dir: PathBuf,
    pub source: ContentInstallSource,
    // ModpackHit::source_key() of the project this file belongs to, so
    // spawn_install_content can record it in the installed-content sidecar
    // and delete any older file it's replacing.
    pub key: String,
    // resource packs are exempt from the superseded-file cleanup below
    // (installing v3 no longer deletes v2 - only mods still replace on
    // reinstall).
    pub kind: ContentKind,
}

pub struct ContentBrowseState {
    pub open: bool,
    pub kind: ContentKind,
    pub instance_name: String,
    pub dest_dir: PathBuf,
    pub game_version: String,
    pub loader: ModLoader,

    pub step: BrowseStep,
    pub source: ModpackSource,
    pub query: TextState<'static>,
    pub query_focused: bool,
    // bumped on every query edit; stale searches check it and drop
    // their results instead of clobbering the newer fetch.
    pub search_generation: u64,
    // skips re-firing the exact same query (Enter after the debounce fired)
    pub last_searched_query: String,
    // current search task; aborted whenever a new search supersedes it
    pub search_task: Option<tokio::task::AbortHandle>,
    pub results: LoadState<Vec<ModpackHit>>,
    pub idx: usize,
    pub versions: LoadState<Vec<ModpackVersionHit>>,
    pub version_idx: usize,
    // true while install_latest's version lookup is in flight — blocks
    // re-triggering 'i' on the same hit. before the popup stayed open, a
    // second press was impossible (it closed); now that it stays open,
    // mashing 'i' would queue duplicate downloads of the same version.
    pub pending_install: bool,
    // ModpackHit::source_key() -> installed filename, for every project
    // already installed in dest_dir. loaded once when the popup opens and
    // updated in place as installs land, so the "Installed" badge and the
    // replace-on-reinstall behavior in spawn_install_content both have
    // something to check against without re-reading the sidecar file.
    pub installed: std::collections::HashMap<String, String>,
}

impl Default for ContentBrowseState {
    fn default() -> Self {
        Self {
            open: false,
            kind: ContentKind::Mod,
            instance_name: String::new(),
            dest_dir: PathBuf::new(),
            game_version: String::new(),
            loader: ModLoader::Vanilla,
            step: BrowseStep::default(),
            source: ModpackSource::default(),
            query: TextState::new(),
            query_focused: true,
            search_generation: 0,
            last_searched_query: String::new(),
            search_task: None,
            results: LoadState::Idle,
            idx: 0,
            versions: LoadState::Idle,
            version_idx: 0,
            pending_install: false,
            installed: std::collections::HashMap::new(),
        }
    }
}

impl ContentBrowseState {
    pub fn selected_hit(&self) -> Option<&ModpackHit> {
        match &self.results {
            LoadState::Loaded(hits) => hits.get(self.idx),
            _ => None,
        }
    }

    pub fn selected_version(&self) -> Option<&ModpackVersionHit> {
        match &self.versions {
            LoadState::Loaded(versions) => versions.get(self.version_idx),
            _ => None,
        }
    }
}

pub fn is_open() -> bool {
    BROWSE_STATE.lock().map(|s| s.open).unwrap_or(false)
}

pub fn clear_installed(key: &str) {
    if let Ok(mut state) = BROWSE_STATE.lock() {
        state.installed.remove(key);
        state.pending_install = false;
    }
    crate::tui::request_redraw();
}

pub fn confirm_installed(key: &str, filename: &str) {
    if let Ok(mut state) = BROWSE_STATE.lock() {
        state.installed.insert(key.to_string(), filename.to_string());
    }
    crate::tui::request_redraw();
}

// re-reads the installed-content sidecar into the open browse popup after
// something outside an install changed it (a mod deleted from the content
// tab). no-op when the popup is closed, or when it's open for a different
// instance's dir than the one that changed.
pub fn refresh_installed(content_dir: &std::path::Path) {
    let Ok(mut state) = BROWSE_STATE.lock() else {
        return;
    };
    if !state.open || state.dest_dir != content_dir {
        return;
    }
    // "pending" placeholders mark in-flight installs that haven't landed
    // on disk (or in the sidecar) yet; the fresh read knows nothing about
    // them, so carry them over or badges would flicker off mid-download.
    let pending_keys: Vec<String> = state
        .installed
        .iter()
        .filter(|(_, f)| f.as_str() == "pending")
        .map(|(k, _)| k.clone())
        .collect();
    state.installed = crate::instance::content::installed_meta::load(content_dir);
    for key in pending_keys {
        state.installed.entry(key).or_insert_with(|| "pending".to_string());
    }
    crate::tui::request_redraw();
}

// opens the popup for a specific instance's mods/resourcepacks dir — called
// from the global keybind handler when 'b' is pressed on a content tab.
pub fn open(kind: ContentKind, instance_name: String, dest_dir: PathBuf, game_version: String, loader: ModLoader) {
    if let Ok(mut state) = BROWSE_STATE.lock() {
        let installed = crate::instance::content::installed_meta::load(&dest_dir);
        *state = ContentBrowseState {
            open: true,
            kind,
            instance_name,
            dest_dir,
            game_version,
            loader,
            installed,
            // starts unfocused: the empty-query search below lands a
            // browsable "top mods" listing immediately, so j/k/arrows
            // navigate it right away instead of being swallowed by the
            // query box. '/' (handle_search_key) refocuses it to search.
            query: TextState::new(),
            query_focused: false,
            ..ContentBrowseState::default()
        };
        ensure_search(&mut state);
    }
}

// returns true once the popup has fully closed, so the caller knows to
// hand focus back to the Content area.
pub fn handle_key(key_event: &KeyEvent) -> bool {
    let mut state = match BROWSE_STATE.lock() {
        Ok(state) => state,
        Err(e) => {
            tracing::error!("Content browse state lock poisoned: {}", e);
            return true;
        }
    };

    match state.step {
        BrowseStep::Search => handle_search_key(&mut state, key_event),
        BrowseStep::Version => handle_version_key(&mut state, key_event),
    }

    !state.open
}

fn close(state: &mut ContentBrowseState) {
    state.open = false;
}

fn handle_search_key(state: &mut ContentBrowseState, key_event: &KeyEvent) {
    if state.query_focused {
        match key_event.code {
            // Esc blurs back to the results list first (the search box
            // "disappears"); a second Esc closes the popup via the
            // unfocused branch below.
            KeyCode::Esc => state.query_focused = false,
            KeyCode::Tab => toggle_source(state),
            // search is live (see schedule_search), so Enter just commits
            // focus back to the list. if the debounce hasn't fired yet,
            // bump the generation and fire immediately so the results are
            // fresh; a no-op if the exact query already fired.
            // Enter and Down both commit focus back to the list - Down is
            // the more natural key to reach for once you're done typing
            // and want to start browsing results without moving a hand
            // over to Enter.
            KeyCode::Enter | KeyCode::Down => {
                state.search_generation += 1;
                ensure_search(state);
                state.query_focused = false;
            }
            _ => {
                state.query.handle_key_event(*key_event);
                schedule_search(state);
            }
        }
        return;
    }

    let result_count = match &state.results {
        LoadState::Loaded(results) => results.len(),
        _ => 0,
    };

    match key_event.code {
        KeyCode::Esc => close(state),
        KeyCode::Tab => toggle_source(state),
        KeyCode::Char('/') => state.query_focused = true,
        KeyCode::Char('j') | KeyCode::Down if result_count > 0 => {
            state.idx = (state.idx + 1).min(result_count.saturating_sub(1));
        }
        // k/Up moves up the list, but at the top it instead hands focus
        // back to the search box - same "walk off the top into the
        // header" shape as the instance rename field.
        KeyCode::Char('k') | KeyCode::Up if state.idx == 0 => {
            state.query_focused = true;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.idx = state.idx.saturating_sub(1);
        }
        // a 40-hit search page is too many to walk one row at a time —
        // PageUp/PageDown jump by PAGE_STEP, Home/End snap to the ends,
        // same shape as the version list below.
        KeyCode::PageDown if result_count > 0 => {
            state.idx = (state.idx + PAGE_STEP).min(result_count.saturating_sub(1));
        }
        KeyCode::PageUp => {
            state.idx = state.idx.saturating_sub(PAGE_STEP);
        }
        // 'h' is a vim-flavored alias for Home - jumps straight back to the
        // top of the results without reaching for a dedicated Home key,
        // which not every keyboard/terminal sends reliably.
        KeyCode::Home | KeyCode::Char('h') => state.idx = 0,
        KeyCode::End if result_count > 0 => state.idx = result_count - 1,
        // 'v' keeps the old Enter behavior (version list for the selected
        // hit); Enter itself now opens the full project description — the
        // rmcl-style markdown page with inline images. 'i' still installs
        // the newest compatible version without opening either.
        KeyCode::Char('i') if !state.pending_install => install_latest(state),
        KeyCode::Char('v') => {
            if state.selected_hit().is_none() {
                return;
            }
            state.versions = LoadState::Idle;
            state.version_idx = 0;
            state.step = BrowseStep::Version;
            ensure_versions_loaded(state);
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            let Some(hit) = state.selected_hit().cloned() else {
                return;
            };
            let source = match &hit {
                ModpackHit::Modrinth(h) => description::DescriptionSource::Modrinth {
                    project_id: h.project_id.clone(),
                },
                ModpackHit::CurseForge(m) => description::DescriptionSource::CurseForge { mod_id: m.id },
            };
            description::open(source, hit.title());
        }
        _ => {}
    }
}

// one-shot: fetch the newest version compatible with this instance's game
// version (and, for mods only, its loader) and install it directly, instead
// of Search → Version → Enter. mirrors ensure_versions_loaded's filtering
// exactly, so "latest" here means the same thing the top of the Version
// list would.
fn install_latest(state: &mut ContentBrowseState) {
    let Some(hit) = state.selected_hit().cloned() else {
        return;
    };
    let key = hit.source_key();
    let kind = state.kind;
    let game_version = state.game_version.clone();
    let loader = (kind == ContentKind::Mod).then_some(state.loader);
    // same as ensure_versions_loaded: don't constrain resource packs to the
    // instance's exact game version.
    let version_filter = (kind == ContentKind::Mod).then(|| game_version.clone());
    let dest_dir = state.dest_dir.clone();
    // the popup stays open after install now, so mark the lookup in flight
    // to stop re-pressing 'i' from queueing duplicate downloads; cleared
    // when the version lookup resolves below.
    state.pending_install = true;
    // surface progress through the global status bar (same as the normal
    // install path) — the popup itself has no status line to write to.
    crate::tui::progress::set_action(format!("Installing latest {}...", kind.label()));
    let state_arc = BROWSE_STATE.clone();
    tokio::spawn(async move {
        let client = crate::net::HttpClient::shared();
        let outcome: Result<ContentInstallSource, String> = match &hit {
            ModpackHit::Modrinth(h) => crate::net::modrinth::get_project_versions(
                &client,
                &h.project_id,
                version_filter.as_deref(),
                loader,
            )
            .await
            .map_err(|e| e.to_string())
            .and_then(|versions| {
                versions
                    .into_iter()
                    .next()
                    .map(ContentInstallSource::Modrinth)
                    .ok_or_else(|| "No version compatible with this instance was found".to_string())
            }),
            ModpackHit::CurseForge(m) => {
                let api_key = crate::config::SETTINGS
                    .curseforge
                    .effective_api_key()
                    .unwrap_or("")
                    .to_string();
                curseforge::get_files(&client, &api_key, m.id, version_filter.as_deref(), loader)
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|resp| {
                        resp.data
                            .into_iter()
                            .next()
                            .map(|file| ContentInstallSource::CurseForge { file })
                            .ok_or_else(|| {
                                "No version compatible with this instance was found".to_string()
                            })
                    })
            }
        };
        match outcome {
            Ok(source) => {
                crate::tui::events::emit(crate::tui::events::UiEvent::ContentInstallConfirmed(
                    ContentInstallParams { dest_dir, source, key: key.clone(), kind },
                ));
                // popup stays open so the user can keep browsing — the
                // download runs in the background and lands in the instance
                // dir on its own (spawn_install_content in event.rs). the
                // badge flips on right away rather than waiting for the
                // download to finish, same as the version-picker path.
                if let Ok(mut s) = state_arc.lock() {
                    s.pending_install = false;
                    s.installed.insert(key, "pending".to_string());
                }
            }
            Err(e) => {
                // the popup has no status line, so failures surface as
                // error toasts like every other install path (see
                // spawn_install_content).
                crate::tui::error_buffer::push_error(crate::tui::error_buffer::ErrorEvent {
                    id: 0,
                    level: tracing::Level::ERROR,
                    message: format!("Install failed: {e}"),
                    pushed_at: std::time::Instant::now(),
                });
                if let Ok(mut s) = state_arc.lock() {
                    s.pending_install = false;
                }
            }
        }
        crate::tui::progress::clear();
        crate::tui::request_redraw();
    });
}

fn toggle_source(state: &mut ContentBrowseState) {
    state.source = match state.source {
        ModpackSource::Modrinth => ModpackSource::CurseForge,
        ModpackSource::CurseForge => ModpackSource::Modrinth,
    };
    state.results = LoadState::Idle;
    state.idx = 0;
    state.versions = LoadState::Idle;
    state.version_idx = 0;
    // any pending debounced search belongs to the old source — invalidate
    // it so it can't fire against the newly-switched catalog.
    state.search_generation += 1;
    // stays unfocused, same reasoning as open(): switching source re-fires
    // an empty-query search that lands a fresh browsable listing, so keep
    // it navigable instead of re-stealing focus into the box.
    state.query_focused = false;
    ensure_search(state);
}

fn handle_version_key(state: &mut ContentBrowseState, key_event: &KeyEvent) {
    let version_count = match &state.versions {
        LoadState::Loaded(versions) => versions.len(),
        _ => 0,
    };

    match key_event.code {
        KeyCode::Esc => close(state),
        KeyCode::Left | KeyCode::Char('b') | KeyCode::Char('h') => state.step = BrowseStep::Search,
        KeyCode::Char('j') | KeyCode::Down if version_count > 0 => {
            state.version_idx = (state.version_idx + 1).min(version_count.saturating_sub(1));
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.version_idx = state.version_idx.saturating_sub(1);
        }
        KeyCode::PageDown if version_count > 0 => {
            state.version_idx = (state.version_idx + PAGE_STEP).min(version_count.saturating_sub(1));
        }
        KeyCode::PageUp => {
            state.version_idx = state.version_idx.saturating_sub(PAGE_STEP);
        }
        KeyCode::Home => state.version_idx = 0,
        KeyCode::End if version_count > 0 => state.version_idx = version_count - 1,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            let Some(version) = state.selected_version() else {
                return;
            };
            let source = match version {
                ModpackVersionHit::Modrinth(v) => ContentInstallSource::Modrinth(v.clone()),
                ModpackVersionHit::CurseForge(file) => {
                    ContentInstallSource::CurseForge { file: file.clone() }
                }
            };
            let key = state.selected_hit().map(|h| h.source_key());
            crate::tui::events::emit(crate::tui::events::UiEvent::ContentInstallConfirmed(
                ContentInstallParams {
                    dest_dir: state.dest_dir.clone(),
                    source,
                    key: key.clone().unwrap_or_default(),
                    kind: state.kind,
                },
            ));
            if let Some(key) = key {
                state.installed.insert(key, "pending".to_string());
            }
            // keep the popup open after installing so the user can keep
            // browsing — the download itself runs in the background
            // (spawn_install_content in event.rs) and lands in the instance
            // dir on its own. snap back to the search results instead of
            // closing: it's the hub for picking the next mod/pack, and
            // staying on this screen would let Enter re-queue the same
            // version by accident.
            state.step = BrowseStep::Search;
            state.version_idx = 0;
            state.versions = LoadState::Idle;
        }
        _ => {}
    }
}

// wait after the last keystroke before searching.
const SEARCH_DEBOUNCE_MS: u64 = 500;

// kill the pending search task so a superseded request never fires.
fn abort_search_task(state: &mut ContentBrowseState) {
    if let Some(handle) = state.search_task.take() {
        handle.abort();
    }
}

// fire the search 500ms after the last keystroke.
fn schedule_search(state: &mut ContentBrowseState) {
    state.search_generation += 1;
    abort_search_task(state);
    let generation = state.search_generation;
    let state_arc = BROWSE_STATE.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS)).await;
        if let Ok(mut s) = state_arc.lock() {
            if s.search_generation == generation {
                ensure_search(&mut s);
            }
        }
    });
    state.search_task = Some(handle.abort_handle());
}

fn ensure_search(state: &mut ContentBrowseState) {
    let query = state.query.value().trim().to_string();
    let errored = matches!(state.results, LoadState::Error(_));
    if query == state.last_searched_query && !matches!(state.results, LoadState::Idle) && !errored {
        return;
    }
    state.last_searched_query = query.clone();
    let generation = state.search_generation;
    let source = state.source;
    let kind = state.kind;
    let game_version = state.game_version.clone();
    // the loader and game-version facets only mean anything for mods. a
    // resource pack doesn't run through fabric/forge/etc, so forcing a
    // loader facet filters out packs that never had a loader tag — and
    // packs are tagged against a looser/older set of versions than mods,
    // so filtering by the instance's exact version hides packs that work
    // fine in practice. only mods get filtered by either.
    let loader = (kind == ContentKind::Mod).then_some(state.loader);
    let search_game_version = (kind == ContentKind::Mod).then(|| game_version.clone());
    state.results = LoadState::Loading;
    state.idx = 0;
    abort_search_task(state);
    let state_arc = BROWSE_STATE.clone();
    let handle = tokio::spawn(async move {
        let client = crate::net::HttpClient::shared();
        let outcome: Result<Vec<ModpackHit>, String> = match source {
            ModpackSource::Modrinth => crate::net::modrinth::search(
                &client,
                &query,
                kind.modrinth_project_type(),
                search_game_version.as_deref(),
                loader,
                0,
                40,
            )
            .await
            .map(|resp| resp.hits.into_iter().map(ModpackHit::Modrinth).collect())
            .map_err(|e| e.to_string()),
            ModpackSource::CurseForge => {
                let api_key = crate::config::SETTINGS
                    .curseforge
                    .effective_api_key()
                    .unwrap_or("")
                    .to_string();
                curseforge::search(
                    &client,
                    &api_key,
                    &query,
                    kind.curseforge_class_id(),
                    search_game_version.as_deref(),
                    loader,
                    0,
                    40,
                )
                .await
                .map(|resp| resp.data.into_iter().map(ModpackHit::CurseForge).collect())
                .map_err(|e| e.to_string())
            }
        };
        if let Ok(mut s) = state_arc.lock() {
            // a newer search has superseded this one (more typing, source
            // switch, etc.) - drop the stale results rather than
            // clobbering the newer fetch's output.
            if s.search_generation != generation {
                return;
            }
            s.results = match outcome {
                // belt-and-suspenders on top of the server-side facet/
                // gameVersion filter: for the Mod tab, drop hits that
                // don't list the current game version among the versions
                // they were built for, rather than trusting the search
                // index caught every case. Modrinth hits carry that list
                // for free (`versions`); CurseForge's search hit doesn't
                // include per-version game versions at all (only its
                // per-file listing does, one API call away), so there's
                // nothing to check client-side there. no-op for resource
                // packs (filter_incompatible_mods only acts on Mod).
                Ok(hits) => LoadState::Loaded(filter_incompatible_mods(kind, &game_version, hits)),
                Err(e) => LoadState::Error(e),
            };
        }
        crate::tui::request_redraw();
    });
    state.search_task = Some(handle.abort_handle());
}

fn filter_incompatible_mods(kind: ContentKind, game_version: &str, hits: Vec<ModpackHit>) -> Vec<ModpackHit> {
    if kind != ContentKind::Mod {
        return hits;
    }
    hits.into_iter()
        .filter(|hit| match hit {
            ModpackHit::Modrinth(h) => h.versions.is_empty() || h.versions.iter().any(|v| v == game_version),
            ModpackHit::CurseForge(_) => true,
        })
        .collect()
}

fn ensure_versions_loaded(state: &mut ContentBrowseState) {
    let game_version = state.game_version.clone();
    // same reasoning as ensure_search: only filter by loader for mods.
    let loader = (state.kind == ContentKind::Mod).then_some(state.loader);
    // resource packs aren't tied to a specific game version the way mods
    // are (packs are tagged against a looser/older set of versions than
    // what actually works in practice), so don't filter the version list
    // by the instance's exact version for that kind - only mods get it.
    let version_filter = (state.kind == ContentKind::Mod).then(|| game_version.clone());
    let query = match state.selected_hit() {
        Some(ModpackHit::Modrinth(hit)) => VersionQuery::Modrinth(hit.project_id.clone()),
        Some(ModpackHit::CurseForge(m)) => VersionQuery::CurseForge(m.id),
        None => return,
    };
    let state_arc = BROWSE_STATE.clone();
    tokio::spawn(async move {
        let client = crate::net::HttpClient::shared();
        let outcome: Result<Vec<ModpackVersionHit>, String> = match query {
            VersionQuery::Modrinth(project_id) => crate::net::modrinth::get_project_versions(
                &client,
                &project_id,
                version_filter.as_deref(),
                loader,
            )
            .await
            .map(|versions| versions.into_iter().map(ModpackVersionHit::Modrinth).collect())
            .map_err(|e| e.to_string()),
            VersionQuery::CurseForge(mod_id) => {
                let api_key = crate::config::SETTINGS
                    .curseforge
                    .effective_api_key()
                    .unwrap_or("")
                    .to_string();
                curseforge::get_files(&client, &api_key, mod_id, version_filter.as_deref(), loader)
                    .await
                    .map(|resp| resp.data.into_iter().map(ModpackVersionHit::CurseForge).collect())
                    .map_err(|e| e.to_string())
            }
        };
        if let Ok(mut s) = state_arc.lock() {
            s.versions = match outcome {
                Ok(v) => {
                    if s.version_idx >= v.len() {
                        s.version_idx = 0;
                    }
                    LoadState::Loaded(v)
                }
                Err(e) => LoadState::Error(e),
            };
        }
        crate::tui::request_redraw();
    });
}

enum VersionQuery {
    Modrinth(String),
    CurseForge(u32),
}
