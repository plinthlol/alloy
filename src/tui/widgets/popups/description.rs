// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// project description popup, ported from rmcl's discovery project page:
// fetches the long-form description (Modrinth markdown `body`, CurseForge
// HTML description — the markdown renderer's normalize_html converts it)
// and renders it with the full tui/widgets/markdown.rs pipeline, inline
// images included. opened from the browse popups with Enter on a search
// result; `v` opens the version list instead (the old Enter behavior).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use image::DynamicImage;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Style},
    widgets::{Block, Clear, Paragraph, StatefulWidget, Widget, Wrap},
};
use ratatui_image::{Resize, StatefulImage, protocol::StatefulProtocol};

use crate::config::theme::THEME;
use crate::net::{MAX_PROVIDER_ASSET_BYTES, HttpClient};
use crate::tui::widgets::markdown::{self, Document};
use crate::tui::request_redraw;
use crate::tui::widgets::popups::keybind_line;
// which project to fetch a body for, in whichever API shape its catalog
// needs. the source_key doubles as the cache key for bodies and images.
#[derive(Debug, Clone)]
pub enum DescriptionSource {
    Modrinth { project_id: String },
    CurseForge { mod_id: u32 },
}

impl DescriptionSource {
    pub fn key(&self) -> String {
        match self {
            DescriptionSource::Modrinth { project_id } => format!("modrinth:{project_id}"),
            DescriptionSource::CurseForge { mod_id } => format!("curseforge:{mod_id}"),
        }
    }
}

pub struct DescriptionState {
    pub open: bool,
    // bumps on every open() so results from a superseded fetch (user closed
    // and reopened a different project quickly) are dropped instead of
    // clobbering the newer popup's contents.
    pub request_id: u64,
    pub source_key: String,
    pub title: String,
    pub document: Option<Document>,
    pub error: Option<String>,
    pub scroll: usize,
    pub max_scroll: usize,

    // gallery mode: (url, title) per image, grid selection + scroll, and a
    // full-image preview overlay flag. protocols cache the terminal-encoded
    // thumbnails per url so the grid renders cheaply after the first frame.
    pub gallery: Vec<GalleryItem>,
    pub gallery_open: bool,
    pub gallery_idx: usize,
    pub gallery_scroll_row: usize,
    // columns the grid was last rendered with, so handle_key can move the
    // selection a full row per j/k press (render and input share one lock,
    // but input can fire before the first render — hence the default).
    pub gallery_cols: usize,
    pub preview_open: bool,
    // urls whose fetch/decode already failed. the grid has no third state
    // without it: a cell whose image is never coming would otherwise spin
    // on "loading..." forever, which reads as "still working" and hides the
    // real failure.
    pub gallery_failed: HashSet<String>,
    gallery_protocols: HashMap<String, StatefulProtocol>,
}

impl Default for DescriptionState {
    fn default() -> Self {
        Self {
            open: false,
            request_id: 0,
            source_key: String::new(),
            title: String::new(),
            document: None,
            error: None,
            scroll: 0,
            max_scroll: 0,
            gallery: Vec::new(),
            gallery_open: false,
            gallery_idx: 0,
            gallery_scroll_row: 0,
            gallery_cols: 1,
            preview_open: false,
            gallery_failed: HashSet::new(),
            gallery_protocols: HashMap::new(),
        }
    }
}

// gallery grid paging step (PageUp/PageDown)
const GRID_PAGE: usize = 9;

static DESCRIPTION_STATE: LazyLock<Arc<Mutex<DescriptionState>>> =
    LazyLock::new(|| Arc::new(Mutex::new(DescriptionState::default())));

const MAX_BODY_CACHE_ENTRIES: usize = 40;
const MAX_IMAGE_CACHE_ENTRIES: usize = 64;

// fetched bodies, so re-opening a project you already viewed is instant.
// key = DescriptionSource::key() -> (title, markdown/html body).
static BODY_CACHE: LazyLock<Mutex<HashMap<String, (String, String, Vec<GalleryItem>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// decoded inline document images, keyed (source_key, image url) — same
// process-wide cache pattern as WEB_ICONS: a project's images are fetched
// once per session no matter how often the popup reopens.
static IMAGE_CACHE: LazyLock<Mutex<HashMap<(String, String), DynamicImage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn is_open() -> bool {
    lock_state().open
}

/// test/debug probe: (open, has_document, error) snapshot.
#[doc(hidden)]
pub fn debug_snapshot() -> (bool, bool, Option<String>) {
    let state = lock_state();
    (state.open, state.document.is_some(), state.error.clone())
}

// poison-proof lock helpers: a panic while holding a description lock (e.g.
// inside markdown::render) would otherwise poison the mutex and silently
// kill the whole feature — open() would return early, is_open() would read
// false, and Enter would appear dead with zero logs. recovering the inner
// data keeps the feature working; the panic itself already surfaced through
// the terminal panic hook.
fn lock_state() -> std::sync::MutexGuard<'static, DescriptionState> {
    DESCRIPTION_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_bodies() -> std::sync::MutexGuard<'static, HashMap<String, (String, String, Vec<GalleryItem>)>> {
    BODY_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_images() -> std::sync::MutexGuard<'static, HashMap<(String, String), DynamicImage>> {
    IMAGE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn open(source: DescriptionSource, fallback_title: &str) {
    tracing::info!("Description open requested for {}", source.key());
    let fallback_title = fallback_title.to_string();
    let key = source.key();
    let request_id = begin_request(&key, &fallback_title);
    // dedicated OS thread with its own single-thread runtime instead of
    // tokio::spawn: the TUI event loop blocks a worker with crossterm's
    // 16ms poll and never yields, and on 2-core machines spawned tasks were
    // observed never getting polled. a plain thread + block_on is immune to
    // all of that — the fetch only talks to global state (see the lock
    // helpers) and its own HTTP client, so it needs nothing from the app
    // runtime.
    std::thread::Builder::new()
        .name("description-fetch".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("description fetch runtime");
            runtime.block_on(fetch(request_id, key, source, fallback_title));
        })
        .expect("description fetch thread");
    tracing::info!("Description fetch thread started");
}

// claims the popup for `key` and drops everything the previous project
// left behind, returning the new request id. split out of open() so the
// reset is testable without spawning the fetch thread (and its network
// call) that follows it.
fn begin_request(key: &str, fallback_title: &str) -> u64 {
    let mut state = lock_state();
    state.request_id = state.request_id.wrapping_add(1);
    state.open = true;
    state.source_key = key.to_owned();
    state.title = fallback_title.to_owned();
    state.document = None;
    state.error = None;
    state.scroll = 0;
    state.max_scroll = 0;
    // the previous project's gallery state must not leak into the new
    // one while the fetch is in flight: render_content checks
    // gallery_open before the loading state, so a stale `true` would
    // show the old project's grid over the new "Loading...".
    state.gallery = Vec::new();
    state.gallery_open = false;
    state.gallery_idx = 0;
    state.gallery_scroll_row = 0;
    state.gallery_cols = 1;
    state.preview_open = false;
    state.gallery_failed.clear();
    state.request_id
}

fn state_gallery_thumbs(gallery: &[GalleryItem]) -> impl Iterator<Item = String> + '_ {
    gallery.iter().map(|item| item.thumb.clone())
}

// gallery: modrinth ships screenshots inside the project response (the
// standalone /gallery routes are write-only, and curseforge's core api has
// no gallery read at all — its screenshots are embedded in the description
// HTML, which normalize_html already renders as images).
//
// the gallery deliberately does NOT get appended to the body: the author
// already curates their gallery separately, and splicing `![title](url)`
// into the markdown made every screenshot show up twice — once in the `g`
// grid and again as a full image in the description — with each copy
// fetched and decoded independently. the grid is the only place they live.
fn gallery_items(gallery: &[crate::net::modrinth::GalleryImage]) -> Vec<GalleryItem> {
    let mut images: Vec<&crate::net::modrinth::GalleryImage> = gallery.iter().collect();
    // the author's own ordering, not the API's response order.
    images.sort_by_key(|image| image.ordering);
    images
        .into_iter()
        .map(|image| GalleryItem {
            // url is a ~350px webp thumbnail, raw_url the full-resolution
            // original; CurseForge-derived entries set both to the same.
            thumb: image.url.clone(),
            raw: image.raw_url.clone().unwrap_or_else(|| image.url.clone()),
            title: image.title.clone(),
            featured: image.featured,
        })
        .collect()
}

// fetch hardening: Document::new runs in a spawned task, where a panic
// would be swallowed silently and the popup would show "Loading..." forever.
// catch it, log it, and surface it as a visible error instead.
fn build_document(title: &str, body: &str) -> Result<Document, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Document::new(title, body)
    }))
    .map_err(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        tracing::error!("Description render panicked: {message}");
        format!("Failed to render description: {message}")
    })
}

async fn fetch(request_id: u64, key: String, source: DescriptionSource, fallback_title: String) {
    tracing::info!("Fetching description for {key}");
    let client = HttpClient::shared();

    // body: cache hit is instant; miss goes to the catalog API. clone out
    // of the cache first — the guard can't be held across the await below.
    let cached = lock_bodies().get(&key).cloned();
    let (title, body, mut gallery) = match cached {
        Some(cached) => cached,
        None => {
            let result = match &source {
                DescriptionSource::Modrinth { project_id } => {
                    crate::net::modrinth::get_project(&client, project_id)
                        .await
                        .map(|p| (p.title, p.body, gallery_items(&p.gallery)))
                        .map_err(|e| e.to_string())
                }
                DescriptionSource::CurseForge { mod_id } => {
                    let api_key = crate::config::SETTINGS
                        .curseforge
                        .effective_api_key()
                        .unwrap_or("")
                        .to_string();
                    crate::net::curseforge::get_description(&client, &api_key, *mod_id)
                        .await
                        .map(|body| (fallback_title.clone(), body, Vec::new()))
                        .map_err(|e| e.to_string())
                }
            };
            match result {
                Ok(pair) => {
                    let mut bodies = lock_bodies();
                    if bodies.len() >= MAX_BODY_CACHE_ENTRIES {
                        bodies.clear();
                    }
                    bodies.insert(key.clone(), pair.clone());
                    pair
                }
                Err(e) => {
                    apply(request_id, &key, |state| state.error = Some(e.to_string()));
                    return;
                }
            }
        }
    };

    // curseforge has no gallery endpoint; its description-embedded images
    // are the closest thing, so let the grid show those (untitled). the
    // document parsed for the extraction is reused as the render document —
    // normalize_html + markdown parse is the slowest local step in this
    // function and running it twice was pure waste.
    let mut parsed = None;
    if gallery.is_empty()
        && let Ok(document) = build_document(&title, &body)
    {
        gallery = document
            .image_urls()
            .into_iter()
            .map(|url| GalleryItem {
                thumb: url.clone(),
                raw: url,
                title: String::new(),
                featured: false,
            })
            .collect();
        parsed = Some(document);
    }

    let mut document = match parsed {
        Some(document) => document,
        None => match build_document(&title, &body) {
            Ok(document) => document,
            Err(message) => {
                apply(request_id, &key, |state| state.error = Some(message));
                request_redraw();
                return;
            }
        },
    };

    // satisfy what we can from the image cache before fetching the rest
    let mut missing: Vec<String> = Vec::new();
    {
        let cache = lock_images();
        for url in document
            .image_urls()
            .into_iter()
            .chain(state_gallery_thumbs(&gallery))
        {
            match cache.get(&(key.clone(), url.clone())) {
                Some(image) => document.set_image(&url, Ok(image.clone())),
                None => missing.push(url),
            }
        }
    }
    // grid thumbnails first: they're tiny and the gallery is what the user
    // opens first; the big full-res description images can trickle in after.
    let thumbs: Vec<String> = state_gallery_thumbs(&gallery).collect();
    // dedup first, THEN apply the thumbs-first priority — an unconditional
    // sort() after sort_by_key() would wipe the priority out.
    missing.sort();
    missing.dedup();
    missing.sort_by_key(|url| !thumbs.contains(url));

    apply(request_id, &key, |state| {
        state.title = title;
        state.document = Some(document);
        state.error = None;
        state.scroll = 0;
        state.max_scroll = 0;
        state.gallery = gallery;
        state.gallery_open = false;
        state.gallery_idx = 0;
        state.gallery_scroll_row = 0;
        state.preview_open = false;
    });
    tracing::info!("Description for {key} ready ({} image(s))", missing.len());
    if missing.is_empty() {
        request_redraw();
        return;
    }

    // bounded fetch of inline images: 8-at-a-time — CDN-backed asset urls
    // tolerate the parallelism and the popup is only usable once the
    // gallery thumbnails land, so throughput here is perceived load time.
    // results applied only if this request still owns the popup (not
    // superseded by another open()).
    let semaphore = Arc::new(tokio::sync::Semaphore::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for url in missing {
        let client = client.clone();
        let semaphore = semaphore.clone();
        tasks.spawn(async move {
            // an imgur page/album url isn't fetchable as-is — rewrite it
            // to the image host so the bytes come back as an image.
            let fetch_url = crate::net::imgur::resolve_image_url(&url);
            let result = async {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|e| e.to_string())?;
                let (bytes, final_url) = client
                    .get_bytes_limited_at(&fetch_url, MAX_PROVIDER_ASSET_BYTES)
                    .await
                    .map_err(|e| e.to_string())?;
                // imgur answers a deleted/blocked image with a 200 that
                // redirects to its "removed" asset. it decodes fine, so
                // check the payload and report a failure instead of
                // rendering the wrong picture.
                if crate::net::imgur::is_placeholder(&final_url, &bytes) {
                    return Err("image is no longer available on imgur".to_owned());
                }
                // decodes stills and multi-frame GIF/WebP alike; the
                // animation's frames are only built when there are more
                // than one, so this stays cheap for the common case.
                tokio::task::spawn_blocking(move || markdown::decode_image_frames(&bytes))
                    .await
                    .map_err(|e| e.to_string())?
            }
            .await;
            (url, result)
        });
    }
    while let Some(task) = tasks.join_next().await {
        let Ok((url, result)) = task else { continue };
        if let Ok(animation) = &result {
            let mut cache = lock_images();
            if cache.len() >= MAX_IMAGE_CACHE_ENTRIES {
                cache.clear();
            }
            // the cache holds the still (grid thumbnails and the gallery
            // preview build from it); frames live in the Document, which is
            // discarded when the popup closes.
            cache.insert((key.clone(), url.clone()), animation.first.clone());
        }
        apply(request_id, &key, |state| {
            match &result {
                Ok(animation) => {
                    state.gallery_failed.remove(&url);
                    if let Some(document) = state.document.as_mut() {
                        document.set_animated_image(&url, animation.clone());
                    }
                }
                Err(error) => {
                    // decode failures are indistinguishable from fetch
                    // failures here, and both just show the fallback icon
                    // in the body — but the grid must stop claiming the
                    // image is on its way.
                    state.gallery_failed.insert(url.clone());
                    if let Some(document) = state.document.as_mut() {
                        tracing::debug!("Failed to decode project image {url}: {error}");
                        document.set_image(&url, Err(error.clone()));
                    }
                }
            }
        });
        request_redraw();
    }
}

// applies a mutation to the popup state only if the popup is still open and
// still showing this request's project (dropped otherwise, like rmcl's
// request_id filter).
fn apply(request_id: u64, key: &str, f: impl FnOnce(&mut DescriptionState)) {
    let mut state = lock_state();
    if state.open && state.request_id == request_id && state.source_key == key {
        f(&mut state);
    }
}

// the description renders *in place* of the browse popup's content — the
// host popup swaps its title/keybinds (see title()/keybinds()) and calls
// render_content() with its content area after drawing its frame. no
// separate overlay popup.

/// what the description view did with a key.
/// one gallery entry: `thumb` (small, for the grid) and `raw` (full
/// resolution, for the preview overlay). modrinth provides both; for
/// curseforge they're the same url (description-embedded images).
/// `featured` is modrinth's author-picked badge, shown as a star caption.
#[derive(Clone)]
pub struct GalleryItem {
    pub thumb: String,
    pub raw: String,
    pub title: String,
    pub featured: bool,
}

pub enum KeyAction {
    /// consumed by the description view (scroll, close, ignored).
    Consumed,
    /// close the description and re-dispatch the key to the underlying
    /// browse popup — `v` (versions) and `i` (install latest) act on the
    /// project the popup still has selected.
    Passthrough,
}

// closes the description view and scrolls it; see KeyAction for the return.
// routed from input.rs before the host popup sees any keys, so the view is
// modal over the browse popup it replaced.
pub fn handle_key(key_event: &crossterm::event::KeyEvent) -> KeyAction {
    use crossterm::event::KeyCode;
    let mut state = lock_state();
    let gallery_len = state.gallery.len();

    // full-image preview: close keys step back to the grid; h/l walk the
    // images without the round-trip through the grid.
    if state.preview_open {
        match key_event.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('b') | KeyCode::Char('q') => {
                state.preview_open = false;
            }
            KeyCode::Char('h') | KeyCode::Left => {
                state.gallery_idx = state.gallery_idx.saturating_sub(1);
            }
            KeyCode::Char('l') | KeyCode::Right if state.gallery_idx + 1 < gallery_len => {
                state.gallery_idx += 1;
            }
            _ => {}
        }
        return KeyAction::Consumed;
    }

    // gallery grid: a 2D grid, so the selection moves like a cursor —
    // j/k (and arrows) a full row, h/l (and arrows) one item, PgUp/PgDn a
    // page, Enter previews, b/Esc/g back.
    if state.gallery_open {
        let cols = state.gallery_cols.max(1);
        match key_event.code {
            KeyCode::Esc | KeyCode::Char('b') | KeyCode::Char('g') => {
                state.gallery_open = false;
                state.gallery_idx = 0;
                state.gallery_scroll_row = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                state.gallery_idx = (state.gallery_idx + cols).min(gallery_len.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                state.gallery_idx = state.gallery_idx.saturating_sub(cols);
            }
            KeyCode::Char('h') | KeyCode::Left => {
                state.gallery_idx = state.gallery_idx.saturating_sub(1);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                state.gallery_idx = (state.gallery_idx + 1).min(gallery_len.saturating_sub(1));
            }
            KeyCode::PageDown => {
                state.gallery_idx =
                    (state.gallery_idx + GRID_PAGE).min(gallery_len.saturating_sub(1));
            }
            KeyCode::PageUp => {
                state.gallery_idx = state.gallery_idx.saturating_sub(GRID_PAGE);
            }
            KeyCode::Enter if gallery_len > 0 => {
                state.preview_open = true;
            }
            // v/i still pass through to the popup's own handlers.
            KeyCode::Char('i') | KeyCode::Char('v') => {
                state.gallery_open = false;
                state.gallery_idx = 0;
                state.gallery_scroll_row = 0;
                return KeyAction::Passthrough;
            }
            _ => {}
        }
        return KeyAction::Consumed;
    }

    match key_event.code {
        KeyCode::Esc | KeyCode::Left | KeyCode::Char('b') | KeyCode::Char('h') => {
            state.open = false;
            KeyAction::Consumed
        }
        // hand these back to the browse popup: it still has this project
        // selected, so its own i/v arms do the right thing.
        KeyCode::Char('i') | KeyCode::Char('v') => {
            state.open = false;
            KeyAction::Passthrough
        }
        KeyCode::Char('g') if gallery_len > 0 => {
            state.gallery_open = true;
            state.gallery_idx = 0;
            state.gallery_scroll_row = 0;
            KeyAction::Consumed
        }
        KeyCode::Char('j') | KeyCode::Down => {
            state.scroll = state.scroll.saturating_add(1).min(state.max_scroll);
            KeyAction::Consumed
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.scroll = state.scroll.saturating_sub(1);
            KeyAction::Consumed
        }
        KeyCode::PageDown | KeyCode::Char('d') => {
            state.scroll = state.scroll.saturating_add(10).min(state.max_scroll);
            KeyAction::Consumed
        }
        KeyCode::PageUp | KeyCode::Char('u') => {
            state.scroll = state.scroll.saturating_sub(10);
            KeyAction::Consumed
        }
        KeyCode::Char('G') | KeyCode::End => {
            state.scroll = state.max_scroll;
            KeyAction::Consumed
        }
        KeyCode::Home => {
            state.scroll = 0;
            KeyAction::Consumed
        }
        _ => KeyAction::Consumed,
    }
}

/// current description title, for the host popup's title bar.
pub fn title() -> String {
    DESCRIPTION_STATE
        .lock()
        .map(|s| s.title.clone())
        .unwrap_or_default()
}

/// keybind footer while the description view is showing.
pub fn keybinds() -> ratatui::text::Line<'static> {
    let state = lock_state();
    if state.preview_open {
        return keybind_line(&[("h/l", " switch"), ("b/Esc", " close")]);
    }
    if state.gallery_open {
        return keybind_line(&[
            ("h/l/j/k", " move"),
            ("PgUp/PgDn", " page"),
            ("Enter", " view"),
            ("b/Esc", " back"),
        ]);
    }
    keybind_line(&[
        ("j/k", " scroll"),
        ("PgUp/PgDn", " page"),
        ("g", " gallery"),
        ("v", " versions"),
        ("i", " install latest"),
        ("b/Esc", " back"),
    ])
}

/// draws the description (or its loading/error state) into `inner`.
/// called by the host browse popup after its frame is drawn —
/// markdown::render needs &mut Frame for the inline images.
pub fn render_content(frame: &mut Frame, inner: Rect, picker: &ratatui_image::picker::Picker) {
    let mut state = lock_state();
    let theme = THEME.as_ref();

    if state.preview_open {
        render_preview(frame, inner, picker, &mut state);
        return;
    }
    if state.gallery_open {
        render_gallery(frame, inner, picker, &mut state);
        return;
    }

    if let Some(error) = &state.error {
        Paragraph::new(error.as_str())
            .style(ratatui::style::Style::default().fg(theme.error()))
            .wrap(Wrap { trim: true })
            .render(inner, frame.buffer_mut());
        return;
    }
    if state.document.is_none() {
        Paragraph::new(format!("Loading {}...", state.title))
            .style(ratatui::style::Style::default().fg(theme.text_dim()))
            .render(inner, frame.buffer_mut());
        return;
    }
    // markdown::render clamps scroll against the real content height and
    // returns it — read scroll before taking the document borrow, write back
    // after it ends.
    let mut scroll = state.scroll;
    let (height, animation_deadline) = if let Some(document) = state.document.as_mut() {
        let height = markdown::render(frame, inner, document, &mut scroll, picker);
        (height, document.next_animation_deadline())
    } else {
        return;
    };
    state.scroll = scroll;
    state.max_scroll = height;
    // an animated GIF/WebP on screen needs a redraw when its next frame is
    // due; without this it would sit on frame 0 until the user pressed a
    // key. request a redraw instead of sleeping, so the render pass stays
    // non-blocking — the event loop's poll cadence paces playback.
    if animation_deadline
        .is_some_and(|deadline| deadline <= std::time::Instant::now() + FRAME_EAGER_WINDOW)
    {
        request_redraw();
    }
}

// an on-screen animation is asked to redraw slightly before its frame is
// actually due, so the next tick is already drawing the new frame rather
// than a tick late. comfortably larger than the event loop's 16ms poll.
const FRAME_EAGER_WINDOW: std::time::Duration = std::time::Duration::from_millis(40);

// grid cell sizing, mirroring screenshots_grid's constraints.
const TARGET_CELL_WIDTH: u16 = 34;
const MIN_CELL_WIDTH: u16 = 24;
const MAX_CELL_WIDTH: u16 = 52;
// image rows + 1 caption row inside each cell
const CELL_IMAGE_ROWS: u16 = 11;
const CELL_CAPTION_ROWS: u16 = 1;
const CELL_HEIGHT: u16 = CELL_IMAGE_ROWS + CELL_CAPTION_ROWS + 1;

fn gallery_image(source_key: &str, url: &str) -> Option<image::DynamicImage> {
    lock_images().get(&(source_key.to_string(), url.to_string())).cloned()
}

// pixel dimensions of a cached image, for the footer/preview info line.
fn cached_dimensions(source_key: &str, url: &str) -> Option<(u32, u32)> {
    lock_images()
        .get(&(source_key.to_string(), url.to_string()))
        .map(|image| (image.width(), image.height()))
}

fn render_gallery(
    frame: &mut Frame,
    inner: Rect,
    picker: &ratatui_image::picker::Picker,
    state: &mut DescriptionState,
) {
    let theme = THEME.as_ref();
    let total = state.gallery.len();
    if total == 0 {
        Paragraph::new("No gallery images.")
            .style(Style::default().fg(theme.text_dim()))
            .render(inner, frame.buffer_mut());
        return;
    }

    // header: provider + count, so the view reads as a gallery page rather
    // than a floating grid; the grid and footer live below it.
    let provider = if state.source_key.starts_with("curseforge:") {
        "CurseForge"
    } else {
        "Modrinth"
    };
    let header = format!(
        "{provider} gallery \u{b7} {total} image{}",
        if total == 1 { "" } else { "s" }
    );
    Paragraph::new(header)
        .style(Style::default().fg(theme.text_dim()))
        .render(
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 1,
            },
            frame.buffer_mut(),
        );
    let grid_area = Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    };

    let min_cols = (grid_area.width / MAX_CELL_WIDTH).max(1) as usize;
    let max_cols = (grid_area.width / MIN_CELL_WIDTH).max(1) as usize;
    let target_cols = (grid_area.width / TARGET_CELL_WIDTH).max(1) as usize;
    let cols = target_cols.clamp(min_cols, max_cols);
    let rows = (grid_area.height.saturating_sub(1) / CELL_HEIGHT).max(1) as usize;
    // remembered for handle_key: j/k move a full row, h/l one item.
    state.gallery_cols = cols;

    // keep the selection inside the visible window
    let sel_row = state.gallery_idx / cols;
    let top_row = state.gallery_scroll_row;
    let top_row = if sel_row < top_row {
        sel_row
    } else if sel_row >= top_row + rows {
        sel_row + 1 - rows
    } else {
        top_row
    };
    state.gallery_scroll_row = top_row;

    let theme2 = THEME.as_ref();
    for r in 0..rows {
        for c in 0..cols {
            let idx = (top_row + r) * cols + c;
            if idx >= total {
                break;
            }
            let (thumb, title, featured) = (
                state.gallery[idx].thumb.clone(),
                state.gallery[idx].title.clone(),
                state.gallery[idx].featured,
            );
            let is_selected = idx == state.gallery_idx;
            let cell = Rect {
                x: grid_area.x + c as u16 * (grid_area.width / cols as u16),
                y: grid_area.y + r as u16 * CELL_HEIGHT,
                width: grid_area.width / cols as u16,
                height: CELL_HEIGHT,
            };
            if cell.width == 0 || cell.right() > grid_area.right() || cell.bottom() > grid_area.bottom() {
                continue;
            }

            // selection frame
            if is_selected {
                Block::bordered()
                    .border_style(Style::default().fg(theme2.accent()))
                    .render(cell, frame.buffer_mut());
            }

            let image_area = Rect {
                x: cell.x + 1,
                y: cell.y + 1,
                width: cell.width.saturating_sub(2),
                height: CELL_IMAGE_ROWS,
            };
            let star = if featured { "\u{2605} " } else { "" };
            let caption = if title.is_empty() {
                format!("{star}{} / {total}", idx + 1)
            } else {
                format!("{star}{title}")
            };
            let caption_area = Rect {
                x: cell.x + 1,
                y: cell.y + 1 + CELL_IMAGE_ROWS,
                width: cell.width.saturating_sub(2),
                height: CELL_CAPTION_ROWS,
            };
            Paragraph::new(caption)
                .style(Style::default().fg(if is_selected {
                    theme2.text()
                } else {
                    theme2.text_dim()
                }))
                .render(caption_area, frame.buffer_mut());

            // protocol built once per url, from the small thumbnail —
            // cloning/encoding the full-res image per frame is what made
            // the grid crawl.
            if !state.gallery_protocols.contains_key(&thumb) {
                if let Some(image) = gallery_image(&state.source_key, &thumb) {
                    let protocol = picker.new_resize_protocol(image);
                    state.gallery_protocols.insert(thumb.clone(), protocol);
                }
            }
            match state.gallery_protocols.get_mut(&thumb) {
                Some(protocol) => {
                    let widget = StatefulImage::default().resize(Resize::Fit(None));
                    StatefulWidget::render(widget, image_area, frame.buffer_mut(), protocol);
                }
                // "loading..." for a url still in flight, and a plain
                // failure label once we know it is never coming.
                None if state.gallery_failed.contains(&thumb) => {
                    Paragraph::new("no image")
                        .style(Style::default().fg(theme2.error()))
                        .render(image_area, frame.buffer_mut());
                }
                None => {
                    Paragraph::new("loading...")
                        .style(Style::default().fg(theme2.text_dim()))
                        .render(image_area, frame.buffer_mut());
                }
            }
        }
    }

    // footer: just the position counter.
    Paragraph::new(format!("{} / {total}", state.gallery_idx + 1))
        .style(Style::default().fg(theme.text_dim()))
        .render(
            Rect {
                x: inner.x,
                y: inner.bottom().saturating_sub(1),
                width: inner.width,
                height: 1,
            },
            frame.buffer_mut(),
        );
}

fn render_preview(
    frame: &mut Frame,
    inner: Rect,
    picker: &ratatui_image::picker::Picker,
    state: &mut DescriptionState,
) {
    let theme = THEME.as_ref();
    Clear.render(inner, frame.buffer_mut());
    let Some(item) = state.gallery.get(state.gallery_idx) else {
        return;
    };
    let (raw, title) = (item.raw.clone(), item.title.clone());
    let block = Block::bordered()
        .title(title.clone())
        .border_style(Style::default().fg(theme.accent()));
    let image_area = {
        let full = block.inner(inner);
        Rect {
            x: full.x,
            y: full.y,
            width: full.width,
            height: full.height.saturating_sub(1),
        }
    };
    block.render(inner, frame.buffer_mut());

    if !state.gallery_protocols.contains_key(&raw) {
        if let Some(image) = gallery_image(&state.source_key, &raw) {
            let protocol = picker.new_resize_protocol(image);
            state.gallery_protocols.insert(raw.clone(), protocol);
        }
    }
    match state.gallery_protocols.get_mut(&raw) {
        Some(protocol) => {
            let widget = StatefulImage::default().resize(Resize::Fit(None));
            StatefulWidget::render(widget, image_area, frame.buffer_mut(), protocol);
        }
        None if state.gallery_failed.contains(&raw) => {
            Paragraph::new("no image")
                .style(Style::default().fg(theme.error()))
                .render(image_area, frame.buffer_mut());
        }
        None => {
            Paragraph::new("loading...")
                .style(Style::default().fg(theme.text_dim()))
                .render(image_area, frame.buffer_mut());
        }
    }

    let hint_text = match cached_dimensions(&state.source_key, &raw) {
        Some((width, height)) => {
            format!("{width}\u{d7}{height}  \u{b7}  {} / {}", state.gallery_idx + 1, state.gallery.len())
        }
        None => format!("{} / {}", state.gallery_idx + 1, state.gallery.len()),
    };
    let hint = Paragraph::new(hint_text)
        .style(Style::default().fg(theme.text_dim()))
        .alignment(ratatui::layout::Alignment::Center);
    hint.render(
        Rect {
            x: inner.x,
            y: inner.bottom().saturating_sub(1),
            width: inner.width,
            height: 1,
        },
        frame.buffer_mut(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // DESCRIPTION_STATE is a process-global static; without serialisation,
    // parallel tests would race between setting the gallery and render
    // re-acquiring the mutex internally. same guard as new_instance's
    // wizard snapshot tests.
    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gallery_item(url: &str, title: &str) -> GalleryItem {
        GalleryItem {
            thumb: url.to_owned(),
            raw: url.to_owned(),
            title: title.to_owned(),
            featured: false,
        }
    }

    fn set_gallery(items: Vec<GalleryItem>, failed: &[&str]) {
        let mut state = lock_state();
        *state = DescriptionState::default();
        state.open = true;
        state.source_key = "modrinth:test-project".to_owned();
        state.gallery_open = true;
        state.gallery = items;
        for url in failed {
            state.gallery_failed.insert((*url).to_owned());
        }
    }

    fn rendered_text() -> String {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let picker = ratatui_image::picker::Picker::halfblocks();
        terminal
            .draw(|frame| render_content(frame, frame.area(), &picker))
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    // a url whose fetch already failed must not sit on "loading..." for the
    // rest of the session — that reads as "still coming" and is the only
    // signal the popup gives about an image it has given up on.
    #[test]
    fn gallery_grid_distinguishes_a_failed_image_from_one_still_in_flight() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_gallery(
            vec![
                gallery_item("https://example.com/dead.webp", "dead"),
                gallery_item("https://example.com/coming.webp", "coming"),
            ],
            &["https://example.com/dead.webp"],
        );

        let text = rendered_text();
        assert!(
            text.contains("no image"),
            "the failed cell must say so, got:\n{text}"
        );
        assert!(
            text.contains("loading..."),
            "the in-flight cell must still say it is loading, got:\n{text}"
        );

        lock_state().open = false;
    }

    // opening a different project must not inherit the last one's failures.
    #[test]
    fn begin_request_clears_the_previous_projects_gallery_failures() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_gallery(
            vec![gallery_item("https://example.com/dead.webp", "dead")],
            &["https://example.com/dead.webp"],
        );

        let request_id = begin_request("modrinth:no-such-project", "No Such Project");

        let state = lock_state();
        assert!(
            state.gallery_failed.is_empty(),
            "gallery failures belong to the previous project"
        );
        assert!(state.gallery.is_empty(), "gallery itself must reset too");
        assert_eq!(state.source_key, "modrinth:no-such-project");
        assert_eq!(request_id, state.request_id);
        drop(state);

        lock_state().open = false;
    }
}
