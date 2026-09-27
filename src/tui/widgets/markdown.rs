// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex};

use html_to_markdown_rs::{
    ConversionOptions, ImageMetadata,
    visitor::{HtmlVisitor, NodeContext, VisitResult, VisitorHandle},
};
use image::DynamicImage;
use pulldown_cmark::{CodeBlockKind, Event, Options as MarkdownOptions, Parser, Tag, TagEnd};
use ratatui::{
    Frame,
    layout::{Alignment, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget},
};
use ratatui_image::{
    Resize,
    picker::ProtocolType,
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};
use tui_markdown::{Options as TuiMarkdownOptions, StyleSheet, from_str_with_options};
use unicode_segmentation::UnicodeSegmentation;

use crate::config::settings::ImageProtocol;
use crate::config::theme::THEME;
use crate::instance::content::{
    IconCell, fallback_icon, make_icon_pixels_from_image, make_icon_quadrants_from_image,
};

const MAX_DOCUMENT_WIDTH: u16 = 110;
const MAX_PROJECT_IMAGE_DIMENSION: u32 = 8192;
const MAX_PROJECT_IMAGE_ALLOCATION: u64 = 64 * 1024 * 1024;

struct TextBlock {
    source: String,
    rendered: Option<TextRender>,
}

struct TextRender {
    width: u16,
    text: Text<'static>,
    height: usize,
    links: Vec<TextLink>,
}

struct TextLink {
    x: u16,
    y: usize,
    width: u16,
    url: String,
}

struct LinkTarget {
    width: usize,
    url: String,
}

struct LinkHit {
    area: Rect,
    url: String,
}

#[derive(Clone)]
struct ImageReference {
    url: String,
    alt: String,
    link: Option<String>,
    size: ImageSizeHint,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ImageSizeHint {
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ImageAlignment {
    #[default]
    Left,
    Center,
}

enum DocumentBlock {
    Text(TextBlock),
    ImageRow {
        images: Vec<ImageReference>,
        alignment: ImageAlignment,
    },
}

enum ImageLoad {
    Pending,
    Ready(DynamicImage),
    Failed,
}

impl DocumentImage {
    /// the still currently held by `load`, for callers that need the
    /// decoded image without disturbing an animation's frame clock.
    fn load_image(&self) -> Option<&DynamicImage> {
        match &self.load {
            ImageLoad::Ready(decoded) => Some(decoded),
            ImageLoad::Pending | ImageLoad::Failed => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ImageRenderKey {
    width: u16,
    height: u16,
    protocol: ProtocolType,
    mode: ImageProtocol,
    // which frame of an animation this encodes. 0 for stills, so a still
    // image's key is unchanged by the animation plumbing.
    frame: usize,
}

struct PreparedImage {
    protocol: Option<SlicedProtocol>,
    raster: Vec<Vec<IconCell>>,
}

struct PreparedImageResult {
    url: String,
    key: ImageRenderKey,
    result: Result<PreparedImage, String>,
}

struct DocumentImage {
    load: ImageLoad,
    prepared: Option<(ImageRenderKey, PreparedImage)>,
    pending: Option<ImageRenderKey>,
    // multi-frame source, kept so an animated GIF/WebP can be re-prepared
    // at whichever frame the clock lands on. `frames` is empty for stills.
    animation: Option<Animation>,
}

impl Default for DocumentImage {
    fn default() -> Self {
        Self {
            load: ImageLoad::Pending,
            prepared: None,
            pending: None,
            animation: None,
        }
    }
}

/// a decoded animation plus the clock that decides which frame to show.
struct Animation {
    frames: Vec<DynamicImage>,
    delays_ms: Vec<u64>,
    /// when the current frame started; `frame` is valid for this instant.
    started: std::time::Instant,
    frame: usize,
}

impl Animation {
    fn new(animation: DecodedAnimation) -> Self {
        Self {
            frames: animation.frames,
            delays_ms: animation.delays_ms,
            started: std::time::Instant::now(),
            frame: 0,
        }
    }

    /// total time through one loop, so the frame clock can wrap cleanly
    /// instead of accumulating per-frame drift.
    fn loop_duration(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.delays_ms
                .iter()
                .copied()
                .filter(|delay| *delay > 0)
                .sum::<u64>()
                .max(1),
        )
    }

    /// the frame to display at `now`, advancing the clock when the frame's
    /// delay has elapsed. returns the frame index and whether the caller
    /// must redraw.
    fn frame_at(&mut self, now: std::time::Instant) -> (usize, bool) {
        if self.frames.len() < 2 {
            return (0, false);
        }
        let total = self.loop_duration();
        let elapsed = now.saturating_duration_since(self.started);
        let mut offset = elapsed.as_millis() as u64;
        // past one full loop the per-frame walk is repeated from the top
        offset %= total.as_millis() as u64;
        let mut index = 0usize;
        for (i, delay) in self.delays_ms.iter().enumerate() {
            if offset < *delay {
                index = i;
                break;
            }
            offset -= *delay;
            index = i;
        }
        let changed = index != self.frame;
        self.frame = index;
        (index, changed)
    }

    /// when the currently-shown frame is due to give way to the next one.
    fn next_deadline(&self, now: std::time::Instant) -> std::time::Instant {
        let total = self.loop_duration();
        // re-base the clock each frame so long sessions can't drift
        let base = self.started + std::time::Duration::from_nanos(
            ((now.saturating_duration_since(self.started).as_nanos() / total.as_nanos())
                * total.as_nanos()) as u64,
        );
        let delay = self
            .delays_ms
            .get(self.frame)
            .copied()
            .filter(|delay| *delay > 0)
            .unwrap_or(100);
        base + std::time::Duration::from_millis(delay)
    }
}

pub struct Document {
    blocks: Vec<DocumentBlock>,
    images: HashMap<String, DocumentImage>,
    prepared_images: Arc<Mutex<Vec<PreparedImageResult>>>,
    link_hits: Vec<LinkHit>,
}

impl Document {
    pub fn new(title: &str, body: &str) -> Self {
        let normalized = normalize_html(body);
        let source = strip_duplicate_title(&normalized.markdown, title);
        let blocks = split_images(
            &source,
            &normalized.image_sizes,
            &normalized.image_alignments,
        );
        let images = blocks
            .iter()
            .flat_map(|block| match block {
                DocumentBlock::ImageRow { images, .. } => images.as_slice(),
                DocumentBlock::Text(_) => &[],
            })
            .map(|image| (image.url.clone(), DocumentImage::default()))
            .collect();
        Self {
            blocks,
            images,
            prepared_images: Arc::new(Mutex::new(Vec::new())),
            link_hits: Vec::new(),
        }
    }

    pub fn image_urls(&self) -> Vec<String> {
        self.images.keys().cloned().collect()
    }

    pub fn set_image(&mut self, url: &str, result: Result<DynamicImage, String>) {
        let Some(image) = self.images.get_mut(url) else {
            return;
        };
        image.load = match result {
            Ok(decoded) => ImageLoad::Ready(decoded),
            Err(error) => {
                tracing::debug!("Failed to load project image {url}: {error}");
                ImageLoad::Failed
            }
        };
        image.prepared = None;
        image.pending = None;
    }

    /// installs a multi-frame decode, keeping the first frame as the
    /// stand-in so layout is stable before playback begins.
    pub fn set_animated_image(&mut self, url: &str, animation: DecodedAnimation) {
        let Some(image) = self.images.get_mut(url) else {
            return;
        };
        let animated = animation.is_animated();
        image.animation = animated.then(|| Animation::new(animation.clone()));
        image.load = ImageLoad::Ready(animation.first);
        image.prepared = None;
        image.pending = None;
    }

    /// the soonest time an on-screen animation will need a redraw.
    ///
    /// the event loop only redraws on input, an explicit request, or its
    /// 1s safety tick, so without this a GIF would sit on its first frame
    /// until the user happened to press a key. returns None when nothing
    /// on screen is animating.
    pub fn next_animation_deadline(&self) -> Option<std::time::Instant> {
        let now = std::time::Instant::now();
        self.images
            .values()
            .filter_map(|image| image.animation.as_ref())
            .map(|animation| animation.next_deadline(now))
            .min()
    }

    pub fn link_at(&self, x: u16, y: u16) -> Option<&str> {
        self.link_hits
            .iter()
            .find(|link| link.area.contains((x, y).into()))
            .map(|link| link.url.as_str())
    }

    fn drain_prepared_images(&mut self) {
        let results = match self.prepared_images.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(_) => return,
        };
        for result in results {
            let Some(image) = self.images.get_mut(&result.url) else {
                continue;
            };
            if image.pending != Some(result.key) {
                continue;
            }
            image.pending = None;
            match result.result {
                Ok(prepared) => image.prepared = Some((result.key, prepared)),
                Err(error) => {
                    tracing::debug!("Failed to prepare project image {}: {error}", result.url);
                }
            }
        }
    }
}

#[derive(Debug)]
struct FoundImage {
    range: Range<usize>,
    url: String,
    alt: String,
    link: Option<String>,
    alignment: ImageAlignment,
}

pub fn image_urls(title: &str, body: &str) -> Vec<String> {
    Document::new(title, body).image_urls()
}

// shared fontdb: building it scans the filesystem, so do it once.
// usvg falls back to the generic serif family for unknown fonts, but
// those slots are empty without fontconfig — so pin real fonts there
// or SVG text just gets skipped.
static SVG_FONTDB: LazyLock<resvg::usvg::fontdb::Database> = LazyLock::new(|| {
    let mut db = resvg::usvg::fontdb::Database::new();
    db.load_system_fonts();
    pin_generic_font_fallbacks(&mut db);
    db
});

// per generic slot, first match wins.
const FALLBACK_SANS: &[&str] = &[
    "DejaVu Sans",
    "Noto Sans",
    "Liberation Sans",
    "Ubuntu",
    "Arial",
    "Helvetica",
];
const FALLBACK_SERIF: &[&str] = &[
    "DejaVu Serif",
    "Noto Serif",
    "Liberation Serif",
    "Times New Roman",
    "Georgia",
];
const FALLBACK_MONO: &[&str] = &[
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "Courier New",
];

// resolves the first installed font out of `names`, returning the
// family name fontdb actually knows it by.
fn first_installed_family(
    db: &resvg::usvg::fontdb::Database,
    names: &[&str],
) -> Option<String> {
    for name in names {
        let id = db.query(&resvg::usvg::fontdb::Query {
            families: &[resvg::usvg::fontdb::Family::Name(name)],
            weight: resvg::usvg::fontdb::Weight::NORMAL,
            stretch: resvg::usvg::fontdb::Stretch::Normal,
            style: resvg::usvg::fontdb::Style::Normal,
        });
        if let Some(id) = id
            && let Some(face) = db.face(id)
            && let Some((family, _)) = face.families.first()
        {
            return Some(family.clone());
        }
    }
    None
}

fn pin_generic_font_fallbacks(db: &mut resvg::usvg::fontdb::Database) {
    let sans = first_installed_family(db, FALLBACK_SANS);
    let serif = first_installed_family(db, FALLBACK_SERIF);
    let mono = first_installed_family(db, FALLBACK_MONO);
    // last resort: whatever font fontdb actually found
    let any = db
        .faces()
        .next()
        .and_then(|face| face.families.first().map(|(family, _)| family.clone()));

    if let Some(family) = sans.as_ref().or_else(|| any.as_ref()) {
        db.set_sans_serif_family(family.clone());
    }
    if let Some(family) = serif.as_ref().or_else(|| any.as_ref()) {
        db.set_serif_family(family.clone());
    }
    if let Some(family) = mono.as_ref().or(sans.as_ref()).or(any.as_ref()) {
        db.set_monospace_family(family.clone());
    }
}

pub fn decode_image(bytes: &[u8]) -> Result<DynamicImage, String> {
    Ok(decode_image_frames(bytes)?.first)
}

/// one decoded animation, flattened for rendering.
///
/// GIF/WebP frames are sub-rectangles that composite over the previous
/// frame, so each is painted onto a full-size canvas at its (left, top)
/// offset. `delays_ms` is per-frame, aligned with `frames`; the last
/// entry's delay is ignored (nothing follows it).
#[derive(Clone)]
pub struct DecodedAnimation {
    /// first frame, so a still preview exists before playback starts.
    pub first: DynamicImage,
    pub frames: Vec<DynamicImage>,
    pub delays_ms: Vec<u64>,
}

impl DecodedAnimation {
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }
}

// GIFs in mod descriptions are usually short demo loops, but a hostile
// upload can carry thousands of frames. cap both the frame count and the
// per-frame pixel budget so a decode can't stall the popup (or the
// background thread feeding it).
const MAX_ANIMATION_FRAMES: usize = 120;
const MAX_ANIMATION_FRAME_BYTES: u64 = 8 * 1024 * 1024;
// browsers clamp very short frame delays to ~100ms; matching that keeps a
// 1ms-delay GIF from spinning the redraw loop.
const MIN_FRAME_DELAY_MS: u64 = 100;

/// decodes still and animated images alike. the animation is a byproduct of
/// normal decoding (the `gif`/`webp` features are already compiled in), so
/// this replaces the old single-frame `reader.decode()`: a still image
/// yields exactly one frame and costs the same as before.
pub fn decode_image_frames(bytes: &[u8]) -> Result<DecodedAnimation, String> {
    if let Ok(format) = image::guess_format(bytes)
        && matches!(format, image::ImageFormat::Gif | image::ImageFormat::WebP)
    {
        return decode_animated(bytes, format);
    }

    let decoded = decode_still_or_svg(bytes)?;
    Ok(DecodedAnimation {
        first: decoded.clone(),
        frames: vec![decoded],
        delays_ms: vec![MIN_FRAME_DELAY_MS],
    })
}

fn decode_animated(bytes: &[u8], format: image::ImageFormat) -> Result<DecodedAnimation, String> {
    use image::AnimationDecoder;
    let decoder_frames: image::Frames<'_> = match format {
        image::ImageFormat::Gif => image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
            .map_err(|error| format!("failed to read GIF: {error}"))?
            .into_frames(),
        image::ImageFormat::WebP => {
            image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes))
                .map_err(|error| format!("failed to read WebP: {error}"))?
                .into_frames()
        }
        _ => return Err("unsupported animation format".to_owned()),
    };

    let mut frames = Vec::new();
    let mut delays_ms = Vec::new();
    let mut canvas_size: Option<(u32, u32)> = None;
    for frame in decoder_frames {
        if frames.len() >= MAX_ANIMATION_FRAMES {
            break;
        }
        let frame = frame.map_err(|error| format!("failed to decode animation frame: {error}"))?;
        let (width, height) = (frame.buffer().width(), frame.buffer().height());
        if width > MAX_PROJECT_IMAGE_DIMENSION || height > MAX_PROJECT_IMAGE_DIMENSION {
            return Err("animation frame dimensions are too large".to_owned());
        }
        // canvas size comes from the first frame; a GIF that changes size
        // mid-stream is malformed, so keep the first and pad the rest.
        let (canvas_width, canvas_height) = *canvas_size.get_or_insert((width, height));
        let allocation = u64::from(canvas_width)
            .checked_mul(u64::from(canvas_height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "animation frame dimensions are too large".to_owned())?;
        if allocation > MAX_ANIMATION_FRAME_BYTES {
            return Err("animation frames are too large".to_owned());
        }
        let mut canvas = image::RgbaImage::new(canvas_width, canvas_height);
        // composite the sub-rectangle at its offset; GIF disposal already
        // happened in the decoder, so a plain copy is correct here.
        let (left, top) = (frame.left().min(canvas_width), frame.top().min(canvas_height));
        for (x, y, pixel) in frame.buffer().enumerate_pixels() {
            if x < canvas_width.saturating_sub(left) && y < canvas_height.saturating_sub(top) {
                canvas.put_pixel(x + left, y + top, *pixel);
            }
        }
        let (numer, denom) = frame.delay().numer_denom_ms();
        let delay = numer
            .checked_div(denom)
            .map_or(MIN_FRAME_DELAY_MS, |ms| u64::from(ms).max(MIN_FRAME_DELAY_MS));
        frames.push(DynamicImage::ImageRgba8(canvas));
        delays_ms.push(delay);
    }

    let Some(first) = frames.first().cloned() else {
        return Err("animation contained no frames".to_owned());
    };
    Ok(DecodedAnimation {
        first,
        frames,
        delays_ms,
    })
}

fn decode_still_or_svg(bytes: &[u8]) -> Result<DynamicImage, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PROJECT_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_PROJECT_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_PROJECT_IMAGE_ALLOCATION);
    reader.limits(limits);
    if let Ok(image) = reader.decode() {
        return Ok(image);
    }
    let mut options = resvg::usvg::Options::default();
    *options.fontdb_mut() = SVG_FONTDB.clone();
    let tree = resvg::usvg::Tree::from_data(bytes, &options).map_err(|error| error.to_string())?;
    let size = tree.size().to_int_size();
    let allocation = u64::from(size.width())
        .checked_mul(u64::from(size.height()))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "SVG image dimensions are too large".to_owned())?;
    if size.width() > MAX_PROJECT_IMAGE_DIMENSION
        || size.height() > MAX_PROJECT_IMAGE_DIMENSION
        || allocation > MAX_PROJECT_IMAGE_ALLOCATION
    {
        return Err("SVG image dimensions are too large".to_owned());
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or_else(|| "SVG image dimensions are too large".to_owned())?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    let pixels = image::RgbaImage::from_raw(size.width(), size.height(), pixmap.take())
        .ok_or_else(|| "SVG renderer returned an invalid pixel buffer".to_owned())?;
    Ok(DynamicImage::ImageRgba8(pixels))
}

fn markdown_options() -> MarkdownOptions {
    MarkdownOptions::ENABLE_TABLES
        | MarkdownOptions::ENABLE_STRIKETHROUGH
        | MarkdownOptions::ENABLE_TASKLISTS
}

#[derive(Default)]
struct NormalizedDocument {
    markdown: String,
    image_sizes: HashMap<String, ImageSizeHint>,
    image_alignments: HashMap<String, ImageAlignment>,
}

fn normalize_html(source: &str) -> NormalizedDocument {
    let mut regions = Vec::new();
    let mut html_block_start = None;
    let mut inline_container = None;
    for (event, range) in Parser::new_ext(source, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(Tag::HtmlBlock) => html_block_start = Some(range.start),
            Event::End(TagEnd::HtmlBlock) => {
                if let Some(start) = html_block_start.take() {
                    regions.push(start..range.end);
                }
            }
            Event::Start(Tag::Paragraph | Tag::Heading { .. }) => {
                inline_container = Some((range.start, false));
            }
            Event::InlineHtml(_) => {
                if let Some((_, contains_html)) = inline_container.as_mut() {
                    *contains_html = true;
                }
            }
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_)) => {
                if let Some((start, true)) = inline_container.take() {
                    regions.push(start..range.end);
                }
            }
            _ => {}
        }
    }
    regions.sort_by_key(|range| range.start);
    regions.dedup();

    let mut normalized = source.to_owned();
    let mut image_sizes = HashMap::new();
    let mut image_alignments = HashMap::new();
    for range in regions.into_iter().rev() {
        let html = &source[range.clone()];
        match convert_html_fragment(html) {
            Ok(fragment) => {
                image_sizes.extend(fragment.image_sizes);
                image_alignments.extend(fragment.image_alignments);
                normalized.replace_range(range, &fragment.markdown);
            }
            Err(error) => {
                tracing::debug!("Failed to convert project HTML: {error}");
            }
        }
    }
    NormalizedDocument {
        markdown: normalized,
        image_sizes,
        image_alignments,
    }
}

fn convert_html_fragment(html: &str) -> Result<NormalizedDocument, String> {
    let image_alignments = Arc::new(Mutex::new(HashMap::new()));
    let visitor: VisitorHandle = Arc::new(Mutex::new(ImageAlignmentVisitor {
        centered: Vec::new(),
        image_alignments: image_alignments.clone(),
    }));
    let options = ConversionOptions {
        compact_tables: true,
        strip_tags: vec!["ins".to_owned(), "u".to_owned()],
        visitor: Some(visitor),
        ..ConversionOptions::default()
    };
    let result = html_to_markdown_rs::convert(&format!("<div>{html}</div>"), Some(options))
        .map_err(|error| error.to_string())?;
    let image_sizes = result
        .metadata
        .images
        .iter()
        .filter_map(image_size_hint)
        .collect();
    let image_alignments = image_alignments
        .lock()
        .map(|alignments| alignments.clone())
        .unwrap_or_default();
    Ok(NormalizedDocument {
        markdown: result.content.unwrap_or_default(),
        image_sizes,
        image_alignments,
    })
}

#[derive(Debug)]
struct ImageAlignmentVisitor {
    centered: Vec<bool>,
    image_alignments: Arc<Mutex<HashMap<String, ImageAlignment>>>,
}

impl HtmlVisitor for ImageAlignmentVisitor {
    fn visit_element_start(&mut self, context: &NodeContext<'_>) -> VisitResult {
        let attributes = context.attributes();
        let centered = self.centered.last().copied().unwrap_or_default()
            || context.tag_name.eq_ignore_ascii_case("center")
            || attributes
                .get("align")
                .is_some_and(|align| align.eq_ignore_ascii_case("center"))
            || attributes.get("style").is_some_and(|style| {
                style.split(';').any(|declaration| {
                    declaration.split_once(':').is_some_and(|(name, value)| {
                        name.trim().eq_ignore_ascii_case("text-align")
                            && value.trim().eq_ignore_ascii_case("center")
                    })
                })
            });
        self.centered.push(centered);
        VisitResult::Continue
    }

    fn visit_element_end(&mut self, _context: &NodeContext<'_>, _output: &str) -> VisitResult {
        self.centered.pop();
        VisitResult::Continue
    }

    fn visit_image(
        &mut self,
        _context: &NodeContext<'_>,
        source: &str,
        _alt: &str,
        _title: Option<&str>,
    ) -> VisitResult {
        if self.centered.last().copied().unwrap_or_default()
            && let Ok(mut alignments) = self.image_alignments.lock()
        {
            alignments.insert(source.to_owned(), ImageAlignment::Center);
        }
        VisitResult::Continue
    }
}

fn image_size_hint(image: &ImageMetadata) -> Option<(String, ImageSizeHint)> {
    let mut hint = image
        .dimensions
        .map(|dimensions| ImageSizeHint {
            width: Some(dimensions.width),
            height: Some(dimensions.height),
        })
        .unwrap_or_default();
    hint.width = hint.width.or_else(|| {
        image
            .attributes
            .get("width")
            .and_then(|value| pixel_size(value))
    });
    hint.height = hint.height.or_else(|| {
        image
            .attributes
            .get("height")
            .and_then(|value| pixel_size(value))
    });
    (hint != ImageSizeHint::default()).then(|| (image.src.clone(), hint))
}

fn pixel_size(value: &str) -> Option<u32> {
    let value = value.trim();
    let value = value.strip_suffix("px").unwrap_or(value).trim();
    (!value.is_empty() && value.chars().all(|character| character.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn strip_duplicate_title(source: &str, title: &str) -> String {
    let mut heading_start = None;
    let mut heading_text = String::new();
    for (event, range) in Parser::new_ext(source, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { .. })
                if source[..range.start].trim().is_empty() && heading_start.is_none() =>
            {
                heading_start = Some(range.start);
            }
            Event::Text(text) | Event::Code(text) if heading_start.is_some() => {
                heading_text.push_str(&text);
            }
            Event::End(TagEnd::Heading(_)) if heading_start.is_some() => {
                if heading_text.trim().eq_ignore_ascii_case(title.trim()) {
                    return source[range.end..].trim_start_matches('\n').to_owned();
                }
                break;
            }
            Event::Start(_) if heading_start.is_none() => break,
            _ => {}
        }
    }
    source.to_owned()
}

fn split_images(
    source: &str,
    image_sizes: &HashMap<String, ImageSizeHint>,
    image_alignments: &HashMap<String, ImageAlignment>,
) -> Vec<DocumentBlock> {
    let mut images = Vec::<FoundImage>::new();
    let mut image_stack = Vec::<FoundImage>::new();
    let mut link_stack = Vec::<(usize, usize, String)>::new();

    for (event, range) in Parser::new_ext(source, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                link_stack.push((range.start, images.len(), dest_url.into_string()));
            }
            Event::End(TagEnd::Link) => {
                if let Some((start, first_image, url)) = link_stack.pop()
                    && first_image < images.len()
                {
                    images[first_image].range.start = start;
                    for image in &mut images[first_image..] {
                        image.link = Some(url.clone());
                    }
                    if let Some(last) = images.last_mut() {
                        last.range.end = range.end;
                    }
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                image_stack.push(FoundImage {
                    range: range.start..range.end,
                    url: dest_url.to_string(),
                    alt: String::new(),
                    link: None,
                    alignment: image_alignments
                        .get(dest_url.as_ref())
                        .copied()
                        .unwrap_or(ImageAlignment::Left),
                });
            }
            Event::Text(text) if !image_stack.is_empty() => {
                if let Some(image) = image_stack.last_mut() {
                    image.alt.push_str(&text);
                }
            }
            Event::End(TagEnd::Image) => {
                if let Some(mut image) = image_stack.pop() {
                    image.range.end = range.end;
                    images.push(image);
                }
            }
            _ => {}
        }
    }

    images.sort_by_key(|image| image.range.start);
    let mut blocks = Vec::new();
    let mut image_row = Vec::new();
    let mut cursor = 0;
    for image in images {
        if image.range.start < cursor
            || image.range.end < image.range.start
            || image.range.end > source.len()
            || !source.is_char_boundary(image.range.start)
            || !source.is_char_boundary(image.range.end)
        {
            tracing::debug!("Ignoring invalid project image source range");
            continue;
        }
        let between = &source[cursor..image.range.start];
        let stays_in_row = !image_row.is_empty()
            && between.trim().is_empty()
            && !between.contains("\n\n")
            && image_row
                .first()
                .is_some_and(|(_, alignment)| *alignment == image.alignment);
        if !stays_in_row {
            push_image_row(&mut blocks, &mut image_row);
            push_text_block(&mut blocks, between);
        }
        image_row.push((
            ImageReference {
                size: image_sizes.get(&image.url).copied().unwrap_or_default(),
                url: image.url,
                alt: image.alt,
                link: image.link,
            },
            image.alignment,
        ));
        cursor = image.range.end;
    }
    push_image_row(&mut blocks, &mut image_row);
    if cursor < source.len() {
        push_text_block(&mut blocks, &source[cursor..]);
    }
    blocks
}

fn push_image_row(
    blocks: &mut Vec<DocumentBlock>,
    row: &mut Vec<(ImageReference, ImageAlignment)>,
) {
    if !row.is_empty() {
        let alignment = row[0].1;
        blocks.push(DocumentBlock::ImageRow {
            images: std::mem::take(row)
                .into_iter()
                .map(|(image, _)| image)
                .collect(),
            alignment,
        });
    }
}

fn push_text_block(blocks: &mut Vec<DocumentBlock>, source: &str) {
    let text = source.trim_matches('\n');
    if !text.trim().is_empty() {
        blocks.push(DocumentBlock::Text(TextBlock {
            source: format!("{text}\n"),
            rendered: None,
        }));
    }
}

fn external_markdown_text(source: &str, width: u16) -> (Text<'static>, Vec<LinkTarget>) {
    let styles = MarkdownStyleSheet;
    let rule_style = styles.table_border();
    let list_marker_style = Style::default().fg(THEME.as_ref().text_dim());
    let table_separator_style = styles.table_border();
    let code_style = styles.code();
    let code_language_style = Style::default()
        .fg(THEME.as_ref().text_dim())
        .bg(THEME.as_ref().surface());
    let links = markdown_links(source);
    let options = TuiMarkdownOptions::new(styles);
    let mut text = own_text(from_str_with_options(source, &options));
    text.lines = render_code_blocks(
        text.lines,
        markdown_code_languages(source),
        width,
        code_style,
        code_language_style,
    );
    text.lines = unbox_tables(text.lines);
    for line in &mut text.lines {
        let line_style = Style::default().fg(THEME.as_ref().text()).patch(line.style);
        line.style = line_style;
        line.spans.retain(|span| {
            let content = span.content.as_ref();
            content != "*"
                && !(span.style.add_modifier.contains(Modifier::UNDERLINED)
                    && content.starts_with("(http")
                    && content.ends_with(')'))
        });
        remove_link_destination(line);
        for span in &mut line.spans {
            span.style = line_style.patch(span.style);
            if span.content.contains("**") {
                span.content = span.content.replace("**", "").into();
            }
        }
    }
    text.lines = wrap_tables(text.lines, width, table_separator_style)
        .into_iter()
        .map(|line| {
            if line.to_string() == "---" {
                Line::styled("─".repeat(usize::from(width)), rule_style)
            } else {
                line
            }
        })
        .flat_map(|line| wrap_list_line(line, width, list_marker_style))
        .flat_map(|line| wrap_line(line, width))
        .collect();
    (text, links)
}

fn markdown_code_languages(source: &str) -> Vec<String> {
    Parser::new_ext(source, markdown_options())
        .filter_map(|event| match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(language))) => {
                Some(language.into_string())
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Indented)) => Some(String::new()),
            _ => None,
        })
        .collect()
}

fn render_code_blocks(
    lines: Vec<Line<'static>>,
    languages: Vec<String>,
    width: u16,
    code_style: Style,
    language_style: Style,
) -> Vec<Line<'static>> {
    let mut output = Vec::new();
    let mut lines = lines.into_iter().peekable();
    let mut languages = languages.into_iter();
    while let Some(line) = lines.next() {
        if line.style != code_style {
            output.push(line);
            continue;
        }
        let mut content = vec![line.to_string()];
        while lines.peek().is_some_and(|line| line.style == code_style) {
            content.push(lines.next().expect("peeked code line").to_string());
        }
        output.extend(code_block_lines(
            &languages.next().unwrap_or_default(),
            &content.join("\n"),
            width,
            code_style,
            language_style,
        ));
    }
    output
}

fn unbox_tables(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .filter_map(|mut line| {
            let text = line.to_string();
            if is_table_border(&text, '┌', '┬', '┐') || is_table_border(&text, '└', '┴', '┘')
            {
                return None;
            }
            if is_table_border(&text, '├', '┼', '┤') {
                strip_outer_chars(&mut line, '├', '┤');
            } else if text.starts_with('│') && text.ends_with('│') {
                strip_outer_chars(&mut line, '│', '│');
            }
            Some(line)
        })
        .collect()
}

fn is_table_border(text: &str, left: char, middle: char, right: char) -> bool {
    text.starts_with(left)
        && text.ends_with(right)
        && text.chars().all(|character| {
            matches!(character, '─')
                || character == left
                || character == middle
                || character == right
        })
}

fn strip_outer_chars(line: &mut Line<'static>, left: char, right: char) {
    if let Some(span) = line.spans.iter_mut().find(|span| !span.content.is_empty()) {
        span.content = span.content.trim_start_matches(left).to_owned().into();
    }
    if let Some(span) = line
        .spans
        .iter_mut()
        .rev()
        .find(|span| !span.content.is_empty())
    {
        span.content = span.content.trim_end_matches(right).to_owned().into();
    }
    line.spans.retain(|span| !span.content.is_empty());
}

fn remove_link_destination(line: &mut Line<'static>) {
    let spans = std::mem::take(&mut line.spans);
    let mut output = Vec::with_capacity(spans.len());
    let mut index = 0;
    while index < spans.len() {
        if index + 2 < spans.len()
            && spans[index].content.ends_with(" (")
            && spans[index + 1]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
            && spans[index + 2].content.starts_with(')')
        {
            let mut before = spans[index].clone();
            before.content = before.content.trim_end_matches(" (").to_owned().into();
            if !before.content.is_empty() {
                output.push(before);
            }
            let mut after = spans[index + 2].clone();
            after.content = after.content.trim_start_matches(')').to_owned().into();
            if !after.content.is_empty() {
                output.push(after);
            }
            index += 3;
        } else {
            output.push(spans[index].clone());
            index += 1;
        }
    }
    line.spans = output;
}

fn own_text(text: Text<'_>) -> Text<'static> {
    Text {
        lines: text
            .lines
            .into_iter()
            .map(|line| Line {
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style))
                    .collect(),
                style: line.style,
                alignment: line.alignment,
            })
            .collect(),
        style: text.style,
        alignment: text.alignment,
    }
}

fn markdown_links(source: &str) -> Vec<LinkTarget> {
    let mut links = Vec::new();
    let mut current = None::<(String, String)>;
    let mut in_table = false;
    for event in Parser::new_ext(source, markdown_options()) {
        match event {
            Event::Start(Tag::Table(_)) => in_table = true,
            Event::End(TagEnd::Table) => in_table = false,
            Event::Start(Tag::Link { dest_url, .. }) if !in_table => {
                current = Some((dest_url.into_string(), String::new()));
            }
            Event::Text(text) | Event::Code(text) if current.is_some() => {
                if let Some((_, label)) = current.as_mut() {
                    label.push_str(&text);
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some((url, label)) = current.take() {
                    links.push(LinkTarget {
                        width: Span::raw(label).width(),
                        url,
                    });
                }
            }
            _ => {}
        }
    }
    links
}

fn wrap_tables(
    lines: Vec<Line<'static>>,
    width: u16,
    separator_style: Style,
) -> Vec<Line<'static>> {
    let mut output = Vec::new();
    let mut lines = lines.into_iter().peekable();
    while let Some(header) = lines.next() {
        let Some(separator) = lines.peek() else {
            output.push(header);
            break;
        };
        let Some(columns) = table_column_count(separator, separator_style) else {
            output.push(header);
            continue;
        };
        let Some(header_cells) = table_cells(&header, columns, separator_style) else {
            output.push(header);
            continue;
        };

        lines.next();
        let mut rows = vec![header_cells];
        while let Some(line) = lines.peek() {
            let Some(cells) = table_cells(line, columns, separator_style) else {
                break;
            };
            rows.push(cells);
            lines.next();
        }
        output.extend(render_table_rows(rows, width, separator_style));
    }
    output
}

fn table_column_count(line: &Line<'_>, separator_style: Style) -> Option<usize> {
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    (!text.is_empty()
        && line.spans.iter().all(|span| span.style == separator_style)
        && text.chars().all(|character| matches!(character, '─' | '┼')))
    .then(|| text.chars().filter(|character| *character == '┼').count() + 1)
}

fn table_cells(
    line: &Line<'static>,
    columns: usize,
    separator_style: Style,
) -> Option<Vec<Vec<Span<'static>>>> {
    let mut cells = vec![Vec::new()];
    for span in &line.spans {
        if span.style == separator_style && span.content.contains('│') {
            cells.push(Vec::new());
        } else {
            cells.last_mut()?.push(span.clone());
        }
    }
    if cells.len() != columns {
        return None;
    }
    for cell in &mut cells {
        trim_span_start(cell);
        trim_span_end(cell);
    }
    Some(cells)
}

fn trim_span_start(spans: &mut Vec<Span<'static>>) {
    while let Some(first) = spans.first_mut() {
        let trimmed = first.content.trim_start_matches(char::is_whitespace);
        if trimmed.is_empty() {
            spans.remove(0);
        } else {
            first.content = trimmed.to_owned().into();
            break;
        }
    }
}

fn trim_span_end(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end_matches(char::is_whitespace);
        if trimmed.is_empty() {
            spans.pop();
        } else {
            last.content = trimmed.to_owned().into();
            break;
        }
    }
}

fn render_table_rows(
    rows: Vec<Vec<Vec<Span<'static>>>>,
    width: u16,
    separator_style: Style,
) -> Vec<Line<'static>> {
    let columns = rows.first().map_or(0, Vec::len);
    if columns == 0 {
        return Vec::new();
    }
    let width = usize::from(width.max(1));
    let spaced_separator = " │ ";
    let compact_separator = "│";
    let separator = if width >= columns.saturating_add(3 * columns.saturating_sub(1)) {
        spaced_separator
    } else {
        compact_separator
    };
    let separator_width = Span::raw(separator).width() * columns.saturating_sub(1);
    if width <= separator_width || width.saturating_sub(separator_width) < columns {
        return rows
            .into_iter()
            .map(|row| Line::from(row.into_iter().flatten().collect::<Vec<Span<'static>>>()))
            .flat_map(|line| wrap_line(line, width as u16))
            .collect();
    }

    let available = width - separator_width;
    let natural = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].iter().map(Span::width).sum::<usize>())
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect::<Vec<_>>();
    let widths = fit_column_widths(&natural, available);
    let mut output = Vec::new();
    for (row_index, row) in rows.into_iter().enumerate() {
        if row_index == 1 {
            output.push(table_rule(&widths, separator, separator_style));
        }
        output.extend(render_table_row(row, &widths, separator, separator_style));
    }
    if output.len() == 1 {
        output.push(table_rule(&widths, separator, separator_style));
    }
    output
}

fn fit_column_widths(natural: &[usize], available: usize) -> Vec<usize> {
    if natural.iter().sum::<usize>() <= available {
        return natural.to_vec();
    }
    let mut widths = vec![1; natural.len()];
    for _ in 0..available.saturating_sub(natural.len()) {
        let Some((column, _)) = natural.iter().zip(&widths).enumerate().max_by(
            |(_, (left_natural, left_width)), (_, (right_natural, right_width))| {
                (*left_natural * *right_width).cmp(&(*right_natural * *left_width))
            },
        ) else {
            break;
        };
        widths[column] += 1;
    }
    widths
}

fn render_table_row(
    cells: Vec<Vec<Span<'static>>>,
    widths: &[usize],
    separator: &str,
    separator_style: Style,
) -> Vec<Line<'static>> {
    let wrapped = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| wrap_styled_spans(cell, *width))
        .collect::<Vec<_>>();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    (0..height)
        .map(|line_index| {
            let mut spans = Vec::new();
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    spans.push(Span::styled(separator.to_owned(), separator_style));
                }
                let cell_line = wrapped[column].get(line_index).cloned().unwrap_or_default();
                let cell_width = cell_line.iter().map(Span::width).sum::<usize>();
                spans.extend(cell_line);
                spans.push(Span::raw(" ".repeat(width.saturating_sub(cell_width))));
            }
            Line::from(spans)
        })
        .collect()
}

fn table_rule(widths: &[usize], separator: &str, style: Style) -> Line<'static> {
    let junction = if separator == " │ " {
        "─┼─"
    } else {
        "┼"
    };
    let mut spans = Vec::new();
    for (column, width) in widths.iter().enumerate() {
        if column > 0 {
            spans.push(Span::styled(junction.to_owned(), style));
        }
        spans.push(Span::styled("─".repeat(*width), style));
    }
    Line::from(spans)
}

fn wrap_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    if line.width() <= width {
        return vec![line];
    }
    wrap_styled_spans(&line.spans, width)
        .into_iter()
        .map(|spans| Line {
            style: line.style,
            alignment: line.alignment,
            spans,
        })
        .collect()
}

fn code_block_lines(
    language: &str,
    content: &str,
    width: u16,
    code_style: Style,
    language_style: Style,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let inner_width = width.saturating_sub(2).max(1);
    let mut lines = Vec::new();
    if !language.is_empty() {
        lines.push(padded_code_line(
            &format!("[{language}]"),
            width,
            language_style,
        ));
    }
    for source_line in content.strip_suffix('\n').unwrap_or(content).split('\n') {
        for line in split_display_width(source_line, inner_width) {
            lines.push(padded_code_line(&line, width, code_style));
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled(" ".repeat(width), code_style));
    }
    lines
}

fn padded_code_line(content: &str, width: usize, style: Style) -> Line<'static> {
    let inner_width = width.saturating_sub(2);
    let content_width = Span::raw(content).width().min(inner_width);
    Line::styled(
        format!(
            " {content}{} ",
            " ".repeat(inner_width.saturating_sub(content_width))
        ),
        style,
    )
}

fn split_display_width(source: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut line_width = 0usize;
    for grapheme in source.graphemes(true) {
        let grapheme_width = Span::raw(grapheme).width();
        if line_width > 0 && line_width.saturating_add(grapheme_width) > width {
            lines.push(String::new());
            line_width = 0;
        }
        lines
            .last_mut()
            .expect("display line exists")
            .push_str(grapheme);
        line_width = line_width.saturating_add(grapheme_width);
    }
    lines
}

fn wrap_list_line(line: Line<'static>, width: u16, marker_style: Style) -> Vec<Line<'static>> {
    let Some(marker) = line
        .spans
        .first()
        .filter(|span| is_list_marker(&span.content))
    else {
        return vec![line];
    };
    let leading_spaces = marker
        .content
        .chars()
        .take_while(|character| character.is_whitespace())
        .count();
    let indent = if leading_spaces == 0 {
        2
    } else {
        leading_spaces
    };
    let marker = format!(
        "{}{} ",
        " ".repeat(indent),
        if marker.content.trim() == "-" {
            "•"
        } else {
            marker.content.trim()
        }
    );
    let marker_width = Span::raw(&marker).width();
    let content_width = usize::from(width).saturating_sub(marker_width).max(1);
    let wrapped = wrap_styled_spans(&line.spans[1..], content_width);
    let continuation = " ".repeat(marker_width);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, mut spans)| {
            spans.insert(
                0,
                Span::styled(
                    if index == 0 {
                        marker.clone()
                    } else {
                        continuation.clone()
                    },
                    marker_style,
                ),
            );
            Line {
                style: line.style,
                alignment: line.alignment,
                spans,
            }
        })
        .collect()
}

fn is_list_marker(marker: &str) -> bool {
    let marker = marker.trim();
    marker == "-"
        || marker == "•"
        || marker
            .strip_suffix('.')
            .is_some_and(|number| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
}

fn wrap_styled_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut lines = Vec::new();
    let mut line = Vec::new();
    let mut line_width = 0usize;
    let mut whitespace = Vec::new();
    let mut whitespace_width = 0usize;

    for span in spans {
        for part in span.content.split_word_bounds() {
            if part.chars().all(char::is_whitespace) {
                push_styled(&mut whitespace, part, span.style);
                whitespace_width = whitespace_width.saturating_add(Span::raw(part).width());
                continue;
            }

            let part_width = Span::raw(part).width();
            if line_width > 0
                && line_width
                    .saturating_add(whitespace_width)
                    .saturating_add(part_width)
                    > width
            {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
                whitespace.clear();
                whitespace_width = 0;
            } else if line_width > 0 {
                line.append(&mut whitespace);
                line_width = line_width.saturating_add(whitespace_width);
                whitespace_width = 0;
            } else {
                whitespace.clear();
                whitespace_width = 0;
            }

            for grapheme in part.graphemes(true) {
                let grapheme_width = Span::raw(grapheme).width();
                if line_width > 0 && line_width.saturating_add(grapheme_width) > width {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                }
                push_styled(&mut line, grapheme, span.style);
                line_width = line_width.saturating_add(grapheme_width);
            }
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

fn push_styled(spans: &mut Vec<Span<'static>>, content: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(content);
    } else {
        spans.push(Span::styled(content.to_owned(), style));
    }
}

#[derive(Clone, Copy)]
struct MarkdownStyleSheet;

impl StyleSheet for MarkdownStyleSheet {
    fn heading(&self, _level: u8) -> Style {
        Style::default()
            .fg(THEME.as_ref().accent())
            .add_modifier(Modifier::BOLD)
    }

    fn code(&self) -> Style {
        Style::default()
            .fg(THEME.as_ref().text())
            .bg(THEME.as_ref().surface())
    }

    fn link(&self) -> Style {
        Style::default()
            .fg(THEME.as_ref().info())
            .add_modifier(Modifier::UNDERLINED)
    }

    fn blockquote(&self) -> Style {
        Style::default().fg(THEME.as_ref().text_dim())
    }

    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }

    fn code_block_fence(&self) -> &str {
        ""
    }

    fn html(&self) -> Style {
        Style::default().fg(THEME.as_ref().text_dim())
    }

    fn table_header(&self) -> Style {
        self.heading(1)
    }

    fn table_cell(&self) -> Style {
        Style::default().fg(THEME.as_ref().text())
    }

    fn table_border(&self) -> Style {
        Style::default().fg(THEME.as_ref().border())
    }
}

impl TextBlock {
    fn prepare(&mut self, width: u16) -> &TextRender {
        if self
            .rendered
            .as_ref()
            .is_none_or(|rendered| rendered.width != width)
        {
            let (text, targets) = external_markdown_text(&self.source, width);
            let height = text.lines.len().max(1);
            let links = layout_links(&text, &targets, width);
            self.rendered = Some(TextRender {
                width,
                text,
                height,
                links,
            });
        }
        self.rendered.as_ref().expect("text was prepared")
    }
}

fn layout_links(text: &Text<'static>, targets: &[LinkTarget], width: u16) -> Vec<TextLink> {
    let mut links = Vec::<TextLink>::new();
    let mut target = 0usize;
    let mut remaining = targets.first().map_or(0, |target| target.width);
    for (y, line) in text.lines.iter().enumerate() {
        let line_width = line.width().min(usize::from(width));
        let mut x = match line.alignment.unwrap_or(Alignment::Left) {
            Alignment::Left => 0,
            Alignment::Center => usize::from(width).saturating_sub(line_width) / 2,
            Alignment::Right => usize::from(width).saturating_sub(line_width),
        };
        for span in &line.spans {
            let is_link = span.style.add_modifier.contains(Modifier::UNDERLINED);
            for grapheme in span.content.graphemes(true) {
                let grapheme_width = Span::raw(grapheme).width();
                if is_link && grapheme_width > 0 {
                    while remaining == 0 && target + 1 < targets.len() {
                        target += 1;
                        remaining = targets[target].width;
                    }
                    if let Some(link) = targets.get(target) {
                        let width = grapheme_width.min(remaining);
                        if width > 0 {
                            if let Some(last) = links.last_mut()
                                && last.y == y
                                && usize::from(last.x) + usize::from(last.width) == x
                                && last.url == link.url
                            {
                                last.width = last.width.saturating_add(width as u16);
                            } else {
                                links.push(TextLink {
                                    x: x.min(usize::from(u16::MAX)) as u16,
                                    y,
                                    width: width.min(usize::from(u16::MAX)) as u16,
                                    url: link.url.clone(),
                                });
                            }
                            remaining = remaining.saturating_sub(width);
                        }
                    }
                }
                x = x.saturating_add(grapheme_width);
            }
        }
    }
    links
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    document: &mut Document,
    scroll: &mut usize,
    picker: &ratatui_image::picker::Picker,
) -> usize {
    if area.width == 0 || area.height == 0 {
        *scroll = 0;
        return 0;
    }
    let content_width = area.width.min(MAX_DOCUMENT_WIDTH);
    let content_area = Rect {
        x: area
            .x
            .saturating_add(area.width.saturating_sub(content_width) / 2),
        width: content_width,
        ..area
    };
    document.drain_prepared_images();
    document.link_hits.clear();
    let pending_preparations = document.prepared_images.clone();
    let (blocks, images, link_hits) = (
        &mut document.blocks,
        &mut document.images,
        &mut document.link_hits,
    );
    let mut heights = Vec::with_capacity(blocks.len());
    let mut image_rows = HashMap::new();
    for (index, block) in blocks.iter_mut().enumerate() {
        let height = match block {
            DocumentBlock::Text(text) => text.prepare(content_area.width).height,
            DocumentBlock::ImageRow {
                images: row,
                alignment,
            } => {
                let layout = image_row_layout(row, *alignment, images, content_area, picker);
                let height = usize::from(layout.height);
                image_rows.insert(index, layout);
                height
            }
        };
        heights.push(height);
    }

    let line_count = heights.iter().sum::<usize>();
    let max_scroll = line_count.saturating_sub(usize::from(area.height));
    *scroll = (*scroll).min(max_scroll);
    let viewport_start = *scroll;
    let viewport_end = viewport_start.saturating_add(usize::from(area.height));
    let mut document_y = 0usize;

    for (index, (block, height)) in blocks.iter_mut().zip(heights).enumerate() {
        let block_end = document_y.saturating_add(height);
        if block_end > viewport_start && document_y < viewport_end {
            let hidden_top = viewport_start.saturating_sub(document_y);
            let visible_y = document_y.saturating_sub(viewport_start);
            let visible_height = height
                .saturating_sub(hidden_top)
                .min(viewport_end.saturating_sub(document_y.max(viewport_start)));
            let block_area = Rect {
                x: content_area.x,
                y: area.y.saturating_add(visible_y as u16),
                width: content_area.width,
                height: visible_height as u16,
            };
            match block {
                DocumentBlock::Text(text) => {
                    let rendered = text.prepare(content_area.width);
                    for link in &rendered.links {
                        let link_y = document_y.saturating_add(link.y);
                        if link_y < viewport_start || link_y >= viewport_end {
                            continue;
                        }
                        let x = content_area.x.saturating_add(link.x);
                        let width = link.width.min(content_area.right().saturating_sub(x));
                        if width > 0 {
                            link_hits.push(LinkHit {
                                area: Rect {
                                    x,
                                    y: area.y.saturating_add(
                                        link_y.saturating_sub(viewport_start) as u16
                                    ),
                                    width,
                                    height: 1,
                                },
                                url: link.url.clone(),
                            });
                        }
                    }
                    let paragraph = Paragraph::new(rendered.text.clone())
                        .style(Style::default().fg(THEME.as_ref().text()))
                        .scroll((hidden_top.min(usize::from(u16::MAX)) as u16, 0));
                    frame.render_widget(paragraph, block_area);
                }
                DocumentBlock::ImageRow { images: row, .. } => {
                    if let Some(layout) = image_rows.get(&index) {
                        render_image_row(
                            frame,
                            block_area,
                            row,
                            layout,
                            images,
                            hidden_top,
                            picker,
                            &pending_preparations,
                            link_hits,
                        );
                    }
                }
            }
        }
        document_y = block_end;
    }

    if max_scroll > 0 {
        let scrollbar_position = (*scroll)
            .saturating_mul(line_count.saturating_sub(1))
            .checked_div(max_scroll)
            .unwrap_or_default();
        let mut scrollbar_state = ScrollbarState::new(line_count)
            .position(scrollbar_position)
            .viewport_content_length(usize::from(area.height));
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .render(area, frame.buffer_mut(), &mut scrollbar_state);
    }
    max_scroll
}

struct ImageRowLayout {
    height: u16,
    width: u16,
    alignment: ImageAlignment,
    items: Vec<(u16, u16)>,
}

fn image_row_layout(
    row: &[ImageReference],
    alignment: ImageAlignment,
    images: &HashMap<String, DocumentImage>,
    area: Rect,
    picker: &ratatui_image::picker::Picker,
) -> ImageRowLayout {
    let gap_width = row.len().saturating_sub(1).min(usize::from(u16::MAX)) as u16;
    let available_width = area.width.saturating_sub(gap_width).max(1);
    let mut items = row
        .iter()
        .map(|reference| {
            images
                .get(&reference.url)
                .map_or((6.min(available_width), 3), |image| {
                    image_dimensions(image, reference.size, available_width, picker)
                })
        })
        .collect::<Vec<_>>();
    let total_width = items
        .iter()
        .map(|(width, _)| u32::from(*width))
        .sum::<u32>();
    if total_width > u32::from(available_width) {
        let scale = f64::from(available_width) / f64::from(total_width);
        for (width, height) in &mut items {
            *width = (f64::from(*width) * scale).floor().max(1.0) as u16;
            *height = (f64::from(*height) * scale).ceil().max(1.0) as u16;
        }
    }
    let width = items
        .iter()
        .map(|(width, _)| *width)
        .sum::<u16>()
        .saturating_add(gap_width);
    let height = items.iter().map(|(_, height)| *height).max().unwrap_or(1);
    ImageRowLayout {
        height,
        width,
        alignment,
        items,
    }
}

fn image_dimensions(
    image: &DocumentImage,
    hint: ImageSizeHint,
    available_width: u16,
    picker: &ratatui_image::picker::Picker,
) -> (u16, u16) {
    let ImageLoad::Ready(decoded) = &image.load else {
        return (6.min(available_width), 3);
    };
    let font = picker.font_size();
    let font_width = u32::from(font.width.max(1));
    let font_height = u32::from(font.height.max(1));
    let decoded_width = decoded.width().max(1);
    let decoded_height = decoded.height().max(1);
    let mut pixel_width = hint
        .width
        .or_else(|| {
            hint.height.map(|height| {
                height
                    .saturating_mul(decoded_width)
                    .div_ceil(decoded_height)
            })
        })
        .unwrap_or(decoded_width)
        .max(1);
    let mut pixel_height = hint
        .height
        .unwrap_or_else(|| {
            pixel_width
                .saturating_mul(decoded_height)
                .div_ceil(decoded_width)
        })
        .max(1);
    let available_pixels = u32::from(available_width.max(1)).saturating_mul(font_width);
    if pixel_width > available_pixels {
        pixel_height = pixel_height
            .saturating_mul(available_pixels)
            .div_ceil(pixel_width);
        pixel_width = available_pixels;
    }
    let width = pixel_width
        .div_ceil(font_width)
        .clamp(1, u32::from(available_width.max(1))) as u16;
    let height = pixel_height
        .div_ceil(font_height)
        .clamp(1, u32::from(u16::MAX)) as u16;
    (width, height)
}

#[allow(clippy::too_many_arguments)]
fn render_image_row(
    frame: &mut Frame,
    area: Rect,
    row: &[ImageReference],
    layout: &ImageRowLayout,
    images: &mut HashMap<String, DocumentImage>,
    hidden_top: usize,
    picker: &ratatui_image::picker::Picker,
    pending: &Arc<Mutex<Vec<PreparedImageResult>>>,
    link_hits: &mut Vec<LinkHit>,
) {
    let row_start = match layout.alignment {
        ImageAlignment::Left => area.x,
        ImageAlignment::Center => area.x + area.width.saturating_sub(layout.width) / 2,
    };
    let mut x = row_start;
    for (reference, (width, height)) in row.iter().zip(&layout.items) {
        let item_y = usize::from(layout.height.saturating_sub(*height));
        let item_end = item_y.saturating_add(usize::from(*height));
        let viewport_start = hidden_top;
        let viewport_end = hidden_top.saturating_add(usize::from(area.height));
        // kick off preparation for off-screen rows too: prepare is a
        // background thread, so by the time the user scrolls here the
        // terminal-encoded image is ready instead of popping in late.
        if item_end <= viewport_start || item_y >= viewport_end {
            if let Some(image) = images.get_mut(&reference.url) {
                ensure_image_prepared(image, reference, *width, *height, picker, pending);
            }
            x = x.saturating_add(*width).saturating_add(1);
            continue;
        }
        if item_end > viewport_start && item_y < viewport_end {
            let item_hidden_top = viewport_start.saturating_sub(item_y);
            let visible_y = item_y.saturating_sub(viewport_start);
            let visible_height = item_end
                .min(viewport_end)
                .saturating_sub(item_y.max(viewport_start));
            let image_area = Rect {
                x,
                y: area.y.saturating_add(visible_y as u16),
                width: *width,
                height: visible_height as u16,
            };
            if let Some(url) = &reference.link {
                link_hits.push(LinkHit {
                    area: image_area,
                    url: url.clone(),
                });
            }
            if let Some(image) = images.get_mut(&reference.url) {
                render_image(
                    frame,
                    image_area,
                    image,
                    reference,
                    item_hidden_top,
                    *height,
                    picker,
                    pending,
                );
            }
        }
        x = x.saturating_add(*width).saturating_add(1);
    }
}

// spawns the background preparation for an image at the size it will be
// drawn at, without touching the viewport — used for off-screen rows so
// images are ready before they scroll into view.
// picks the pixels to draw right now: the animation's current frame when
// this image has one, otherwise the single decoded still. also advances
// the frame clock and reports whether the frame changed, so the caller can
// ask for a redraw only when something actually moved.
// picks the pixels to draw right now: the animation's current frame when
// this image has one, otherwise the single decoded still. also advances
// the frame clock and reports whether the frame changed, so the caller can
// ask for a redraw only when something actually moved.
//
// returns a clone because the caller has to write back to the same
// `DocumentImage` (pending/prepared) right after, and a borrow of the
// frame would conflict. Cloning is one decoded still-sized buffer, on a
// path that already clones the image to hand it to the prepare thread.
fn current_frame(image: &mut DocumentImage) -> Option<(DynamicImage, usize, bool)> {
    match &mut image.animation {
        Some(animation) => {
            let (index, changed) = animation.frame_at(std::time::Instant::now());
            Some((animation.frames[index].clone(), index, changed))
        }
        None => image
            .load_image()
            .map(|decoded| (decoded.clone(), 0, false)),
    }
}

fn ensure_image_prepared(
    image: &mut DocumentImage,
    reference: &ImageReference,
    width: u16,
    height: u16,
    picker: &ratatui_image::picker::Picker,
    pending: &Arc<Mutex<Vec<PreparedImageResult>>>,
) {
    if !matches!(&image.load, ImageLoad::Ready(_)) {
        return;
    }
    let key = ImageRenderKey {
        width,
        height,
        protocol: picker.protocol_type(),
        mode: crate::config::SETTINGS.ui.image_protocol,
        frame: image.animation.as_ref().map_or(0, |a| a.frame),
    };
    if image
        .prepared
        .as_ref()
        .is_none_or(|(prepared_key, _)| *prepared_key != key)
        && image.pending != Some(key)
    {
        let (decoded, _, _) = current_frame(image).expect("ready image has pixels");
        image.pending = Some(key);
        prepare_image(
            reference.url.clone(),
            decoded.clone(),
            key,
            picker.clone(),
            pending.clone(),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_image(
    frame: &mut Frame,
    area: Rect,
    image: &mut DocumentImage,
    reference: &ImageReference,
    hidden_top: usize,
    full_height: u16,
    picker: &ratatui_image::picker::Picker,
    pending: &Arc<Mutex<Vec<PreparedImageResult>>>,
) {
    let Some((decoded, frame_index, frame_changed)) = current_frame(image) else {
        render_fallback(frame, area, hidden_top, &reference.alt);
        return;
    };
    // a frame turn means the next prepared image is stale; drop it so the
    // re-prepare below actually runs instead of reusing the old encoding.
    if frame_changed {
        image.prepared = None;
    }
    let key = ImageRenderKey {
        width: area.width,
        height: full_height,
        protocol: picker.protocol_type(),
        mode: crate::config::SETTINGS.ui.image_protocol,
        frame: frame_index,
    };
    if image
        .prepared
        .as_ref()
        .is_none_or(|(prepared_key, _)| *prepared_key != key)
        && image.pending != Some(key)
    {
        image.pending = Some(key);
        prepare_image(
            reference.url.clone(),
            decoded.clone(),
            key,
            picker.clone(),
            pending.clone(),
        );
    }

    let Some((prepared_key, prepared)) = image.prepared.as_ref() else {
        render_fallback(frame, area, hidden_top, &reference.alt);
        return;
    };
    if *prepared_key != key {
        render_fallback(frame, area, hidden_top, &reference.alt);
        return;
    }

    if let Some(protocol) = prepared.protocol.as_ref() {
        let hidden_top = hidden_top.min(i16::MAX as usize) as i16;
        frame.render_widget(
            SlicedImage::new(protocol, SignedPosition::from((0, -hidden_top))),
            area,
        );
    } else {
        render_raster(frame, area, &prepared.raster, hidden_top);
    }
}

fn prepare_image(
    url: String,
    image: DynamicImage,
    key: ImageRenderKey,
    picker: ratatui_image::picker::Picker,
    pending: Arc<Mutex<Vec<PreparedImageResult>>>,
) {
    std::thread::spawn(move || {
        let result = prepare_image_render(image, key, &picker);
        if let Ok(mut pending) = pending.lock() {
            pending.push(PreparedImageResult { url, key, result });
        }
        crate::tui::request_redraw();
    });
}

fn prepare_image_render(
    image: DynamicImage,
    key: ImageRenderKey,
    picker: &ratatui_image::picker::Picker,
) -> Result<PreparedImage, String> {
    let raster = match key.mode {
        ImageProtocol::Quadrants if key.protocol == ProtocolType::Halfblocks => {
            make_icon_quadrants_from_image(&image, key.width, key.height)
        }
        _ => make_icon_pixels_from_image(&image, key.width, key.height),
    };
    let protocol = if key.protocol == ProtocolType::Halfblocks {
        None
    } else {
        let image = bottom_align_to_cells(image, key, picker);
        Some(
            SlicedProtocol::new_with_resize(
                picker,
                image,
                Size::new(key.width, key.height),
                Resize::Fit(None),
            )
            .map_err(|error| error.to_string())?,
        )
    };
    Ok(PreparedImage { protocol, raster })
}

fn bottom_align_to_cells(
    image: DynamicImage,
    key: ImageRenderKey,
    picker: &ratatui_image::picker::Picker,
) -> DynamicImage {
    let font = picker.font_size();
    let width = u32::from(key.width) * u32::from(font.width.max(1));
    let height = u32::from(key.height) * u32::from(font.height.max(1));
    let fitted = image.resize(width, height, image::imageops::FilterType::Lanczos3);
    if fitted.width() == width && fitted.height() == height {
        return fitted;
    }

    let mut canvas = image::RgbaImage::new(width, height);
    image::imageops::overlay(
        &mut canvas,
        &fitted,
        0,
        i64::from(height.saturating_sub(fitted.height())),
    );
    DynamicImage::ImageRgba8(canvas)
}

fn render_fallback(frame: &mut Frame, area: Rect, hidden_top: usize, alt: &str) {
    let fallback = fallback_icon();
    render_raster(frame, area, &fallback, hidden_top);
    if !alt.is_empty() && area.width > 8 {
        let label_area = Rect {
            x: area.x.saturating_add(7),
            width: area.width.saturating_sub(7),
            ..area
        };
        frame.render_widget(
            Paragraph::new(alt.to_owned()).style(Style::default().fg(THEME.as_ref().text_dim())),
            label_area,
        );
    }
}

fn render_raster(frame: &mut Frame, area: Rect, raster: &[Vec<IconCell>], hidden_top: usize) {
    for (visible_row, cells) in raster
        .iter()
        .skip(hidden_top)
        .take(usize::from(area.height))
        .enumerate()
    {
        let line = Line::from(
            cells
                .iter()
                .map(|cell| {
                    Span::styled(
                        cell.symbol.to_string(),
                        Style::default()
                            .fg(Color::Rgb(cell.fg_r, cell.fg_g, cell.fg_b))
                            .bg(Color::Rgb(cell.bg_r, cell.bg_g, cell.bg_b)),
                    )
                })
                .collect::<Vec<_>>(),
        );
        frame.render_widget(
            line,
            Rect {
                x: area.x,
                y: area.y.saturating_add(visible_row as u16),
                width: area.width,
                height: 1,
            },
        );
    }
}

#[cfg(test)]
#[path = "../tests/widgets/markdown.rs"]
mod tests;
