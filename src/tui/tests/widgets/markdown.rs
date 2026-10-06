// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// unit tests for the markdown document pipeline in tui/widgets/markdown.rs.
// kept here (via #[path]) rather than inline so the 1800-line widget stays
// focused on rendering; this module is a child of `markdown`, so private
// helpers are directly testable.

use super::*;

// --- strip_duplicate_title -------------------------------------------------

#[test]
fn strip_duplicate_title_removes_leading_h1_match() {
    let out = strip_duplicate_title("# Sodium\n\nReal body here.\n", "Sodium");
    assert_eq!(out, "Real body here.\n");
}

#[test]
fn strip_duplicate_title_is_case_insensitive() {
    let out = strip_duplicate_title("# SODIUM\n\nbody", "sodium");
    assert_eq!(out, "body");
}

#[test]
fn strip_duplicate_title_keeps_non_matching_heading() {
    let source = "# Something Else\n\nbody";
    assert_eq!(strip_duplicate_title(source, "Sodium"), source);
}

// --- split_images ----------------------------------------------------------

fn image_urls_of(blocks: &[DocumentBlock]) -> Vec<String> {
    blocks
        .iter()
        .flat_map(|block| match block {
            DocumentBlock::ImageRow { images, .. } => {
                images.iter().map(|i| i.url.clone()).collect::<Vec<_>>()
            }
            DocumentBlock::Text(_) => Vec::new(),
        })
        .collect()
}

#[test]
fn split_images_extracts_image_urls() {
    let blocks = split_images(
        "intro\n\n![alt](https://example.com/a.png)\n\noutro",
        &Default::default(),
        &Default::default(),
    );
    assert_eq!(image_urls_of(&blocks), vec!["https://example.com/a.png"]);
}

#[test]
fn split_images_groups_adjacent_images_into_one_row() {
    let blocks = split_images(
        "![](https://a/1.png) ![](https://a/2.png)",
        &Default::default(),
        &Default::default(),
    );
    let rows = blocks
        .iter()
        .filter(|b| matches!(b, DocumentBlock::ImageRow { .. }))
        .count();
    assert_eq!(rows, 1, "adjacent images belong in a single row");
}

#[test]
fn split_images_keeps_linked_image_range_valid() {
    let blocks = split_images(
        "[![alt](https://a/1.png)](https://example.com)",
        &Default::default(),
        &Default::default(),
    );
    let urls = image_urls_of(&blocks);
    assert_eq!(urls, vec!["https://a/1.png"]);
    match &blocks[0] {
        DocumentBlock::ImageRow { images, .. } => {
            assert_eq!(images[0].link.as_deref(), Some("https://example.com"));
        }
        DocumentBlock::Text(_) => panic!("expected an image row"),
    }
}

// --- pixel_size ------------------------------------------------------------

#[test]
fn pixel_size_parses_digits_and_px_suffix() {
    assert_eq!(pixel_size("640"), Some(640));
    assert_eq!(pixel_size("640px"), Some(640));
    assert_eq!(pixel_size(" 100% "), None);
    assert_eq!(pixel_size(""), None);
}

// --- text helpers ----------------------------------------------------------

#[test]
fn split_display_width_never_exceeds_width() {
    for line in split_display_width("hello world, this is long", 7) {
        assert!(Span::raw(&line).width() <= 7);
    }
}

#[test]
fn is_list_marker_matches_expected_shapes() {
    assert!(is_list_marker("-"));
    assert!(is_list_marker("•"));
    assert!(is_list_marker("1."));
    assert!(!is_list_marker("- item"));
    assert!(!is_list_marker("abc."));
}

#[test]
fn wrap_styled_spans_wraps_overlong_single_span() {
    let spans = vec![Span::raw("aaaa bbbb cccc")];
    let lines = wrap_styled_spans(&spans, 9);
    assert!(lines.len() >= 2);
    assert!(lines
        .iter()
        .all(|line| line.iter().map(Span::width).sum::<usize>() <= 9));
}

// --- decode_image ----------------------------------------------------------

#[test]
fn decode_image_parses_a_png() {
    let png = image::DynamicImage::new_rgb8(4, 4);
    let mut bytes = std::io::Cursor::new(Vec::new());
    png.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    let decoded = decode_image(bytes.get_ref()).expect("png decodes");
    assert_eq!((decoded.width(), decoded.height()), (4, 4));
}

#[test]
fn decode_image_rejects_garbage() {
    assert!(decode_image(b"not an image at all").is_err());
}

// --- animated gif ---------------------------------------------------------

// builds a real multi-frame GIF (2x2, distinct per-frame colour) so the
// animation path is exercised against the real encoder, not a stub.
fn animated_gif_bytes(frames: u8) -> Vec<u8> {
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, Rgba, RgbaImage};
    let mut out = Vec::new();
    let mut encoder = GifEncoder::new(&mut out);
    encoder
        .set_repeat(Repeat::Infinite)
        .expect("gif encoder takes repeat");
    for index in 0..frames.max(1) {
        let buffer = RgbaImage::from_pixel(2, 2, Rgba([index * 40, 10, 20, 255]));
        // 200ms, above MIN_FRAME_DELAY_MS so it survives verbatim
        let frame = Frame::from_parts(buffer, 0, 0, Delay::from_numer_denom_ms(200, 1));
        encoder.encode_frame(frame).expect("gif frame encodes");
    }
    drop(encoder); // flushes the trailer byte
    out
}

#[test]
fn decode_image_returns_first_frame_of_an_animated_gif() {
    // the single-frame entry point is used for thumbnails/icon paths, so
    // it must keep working and must not blow up on animation.
    let decoded = decode_image(&animated_gif_bytes(3)).expect("animated gif decodes");
    assert_eq!((decoded.width(), decoded.height()), (2, 2));
}

#[test]
fn decode_image_frames_keeps_every_gif_frame() {
    let animation = decode_image_frames(&animated_gif_bytes(4)).expect("gif decodes");
    assert_eq!(animation.frames.len(), 4);
    assert!(animation.is_animated());
    assert_eq!(animation.delays_ms.len(), 4);
    // first pixel of each frame differs, so frames really are distinct
    let firsts: Vec<[u8; 4]> = animation
        .frames
        .iter()
        .map(|f| f.to_rgba8().get_pixel(0, 0).0)
        .collect();
    assert_eq!(firsts.len(), 4);
    assert!(
        firsts.windows(2).all(|w| w[0] != w[1]),
        "frames should differ: {firsts:?}"
    );
}

#[test]
fn decode_image_frames_treats_a_single_frame_gif_as_still() {
    let animation = decode_image_frames(&animated_gif_bytes(1)).expect("gif decodes");
    assert_eq!(animation.frames.len(), 1);
    assert!(!animation.is_animated());
}

#[test]
fn decode_image_frames_reports_a_still_png_as_one_frame() {
    let animation =
        decode_image_frames(&png_bytes(6, 4)).expect("png decodes as a single frame");
    assert_eq!(animation.frames.len(), 1);
    assert!(!animation.is_animated());
    assert_eq!((animation.first.width(), animation.first.height()), (6, 4));
}

// --- static webp -----------------------------------------------------------

// a real, *non-animated* webp. this is the shape of every Modrinth gallery
// thumbnail (`<hash>_350.webp`), which is why getting it wrong blanks the
// entire grid.
fn static_webp_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        width,
        height,
        image::Rgba([12, 200, 90, 255]),
    ));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::WebP)
        .expect("webp encodes");
    bytes.into_inner()
}

#[test]
fn decode_image_frames_reports_a_still_webp_as_one_frame() {
    // regression: a static WebP reports num_frames == 0, so the animation
    // decoder yields nothing at all. this used to come back as
    // Err("animation contained no frames"), which marked every gallery
    // thumbnail failed and left the grid on "loading..." forever.
    let bytes = static_webp_bytes(8, 6);
    assert!(
        matches!(image::guess_format(&bytes), Ok(image::ImageFormat::WebP)),
        "the fixture really is a webp"
    );

    let animation = decode_image_frames(&bytes).expect("static webp decodes");
    assert_eq!(animation.frames.len(), 1);
    assert!(!animation.is_animated());
    assert_eq!(
        (animation.first.width(), animation.first.height()),
        (8, 6),
        "dimensions must survive the still fallback"
    );
}

#[test]
fn decode_image_still_entry_point_handles_a_static_webp() {
    let decoded = decode_image(&static_webp_bytes(5, 5)).expect("static webp decodes");
    assert_eq!((decoded.width(), decoded.height()), (5, 5));
}

#[test]
fn gif_frame_delays_are_clamped_to_a_playable_floor() {
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba, RgbaImage};
    let mut out = Vec::new();
    let mut encoder = GifEncoder::new(&mut out);
    // 1ms delay: browsers clamp this, and so must we or the redraw loop
    // spins on a GIF that asked for 1000fps.
    let frame = Frame::from_parts(
        RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])),
        0,
        0,
        Delay::from_numer_denom_ms(1, 1),
    );
    encoder.encode_frame(frame).expect("frame encodes");
    drop(encoder);

    let animation = decode_image_frames(&out).expect("gif decodes");
    assert_eq!(animation.delays_ms, vec![MIN_FRAME_DELAY_MS]);
}

// --- animation clock ------------------------------------------------------

#[test]
fn animation_clock_advances_through_frames_and_wraps() {
    let mut animation = Animation::new(decode_image_frames(&animated_gif_bytes(3)).unwrap());
    assert!(animation.frames.len() > 1, "3 frames is animated");

    let start = std::time::Instant::now();
    let (first, _) = animation.frame_at(start);
    assert_eq!(first, 0, "starts on frame 0");

    // 200ms per frame: at 250ms we must be on frame 1.
    let (second, changed) = animation.frame_at(start + std::time::Duration::from_millis(250));
    assert_eq!(second, 1);
    assert!(changed, "a frame turn should report a change");

    // 650ms: one full loop is 3*200=600ms, so we're 50ms into loop 2 —
    // i.e. back on frame 0.
    let (wrapped, _) = animation.frame_at(start + std::time::Duration::from_millis(650));
    assert_eq!(wrapped, 0, "650ms is 50ms into the second loop");

    let (looped, _) = animation.frame_at(start + std::time::Duration::from_millis(850));
    assert_eq!(looped, 1, "250ms into the second loop is frame 1");
}

#[test]
fn single_frame_animation_never_advances() {
    let mut animation = Animation::new(decode_image_frames(&animated_gif_bytes(1)).unwrap());
    let now = std::time::Instant::now();
    assert_eq!(animation.frame_at(now).0, 0);
    assert!(
        !animation.frame_at(now + std::time::Duration::from_secs(10)).1,
        "a still must never report a frame change"
    );
}

#[test]
fn animation_next_deadline_is_in_the_future_and_near_the_frame_delay() {
    let mut animation = Animation::new(decode_image_frames(&animated_gif_bytes(4)).unwrap());
    let now = std::time::Instant::now();
    let _ = animation.frame_at(now);
    let deadline = animation.next_deadline(now);
    assert!(
        deadline > now,
        "deadline {deadline:?} should be after {now:?}"
    );
    assert!(
        deadline <= now + std::time::Duration::from_millis(250),
        "deadline should land within this frame's 200ms delay plus slack"
    );
}

#[test]
fn svg_fontdb_generic_families_resolve() {
    // the whole point of SVG_FONTDB: when the system has any fonts at
    // all, the generic serif slot (usvg's built-in last fallback for
    // unmatchable font-family lists) must resolve — otherwise SVG text
    // is silently skipped with "No match for ... font-family" warnings.
    let db = &*crate::tui::widgets::markdown::SVG_FONTDB;
    if db.faces().next().is_none() {
        return; // fontless system (bare container): nothing to guarantee
    }
    for generic in [
        resvg::usvg::fontdb::Family::Serif,
        resvg::usvg::fontdb::Family::SansSerif,
        resvg::usvg::fontdb::Family::Monospace,
    ] {
        let id = db.query(&resvg::usvg::fontdb::Query {
            families: &[generic],
            weight: resvg::usvg::fontdb::Weight::NORMAL,
            stretch: resvg::usvg::fontdb::Stretch::Normal,
            style: resvg::usvg::fontdb::Style::Normal,
        });
        assert!(id.is_some(), "generic family {generic:?} must resolve");
    }
}

#[test]
fn decode_image_renders_svg_with_unavailable_fonts() {
    // SVGs in the wild ask for fonts that don't exist on the rendering
    // system (Verdana/Geneva on Linux). the render must still succeed and
    // fall back to a pinned generic font rather than dropping the text.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32">
        <text x="2" y="20" font-family="'Verdana', 'Geneva', 'DejaVu Sans', sans-serif" font-size="12">Hi</text>
    </svg>"#;
    let decoded = decode_image(svg).expect("svg with unknown fonts decodes");
    assert_eq!((decoded.width(), decoded.height()), (64, 32));
    // text was actually rasterized: some pixels in the text area are opaque
    let image = decoded.to_rgba8();
    let has_ink = image
        .enumerate_pixels()
        .any(|(_, _, px)| px.0[3] > 0);
    assert!(has_ink, "expected the fallback font to leave visible glyphs");
}

// --- viewport image preparation -------------------------------------------

fn test_picker() -> ratatui_image::picker::Picker {
    ratatui_image::picker::Picker::halfblocks()
}

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        width,
        height,
        image::Rgba([200, 40, 90, 255]),
    ));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).expect("png encodes");
    bytes.into_inner()
}

fn document_with_images(count: usize) -> Document {
    let body = (0..count)
        .map(|i| format!("![img{i}](https://example.com/{i}.png)\n\n"))
        .collect::<String>();
    let mut document = Document::new("Title", &body);
    for i in 0..count {
        let url = format!("https://example.com/{i}.png");
        let decoded = decode_image(&png_bytes(48, 32)).expect("png decodes");
        document.set_image(&url, Ok(decoded));
    }
    document
}

// Regression: an image whose row sits entirely inside the visible viewport
// must be prepared immediately, on the frame that draws it. It used to be
// skipped, because the off-screen "pre-warm" branch tested the row's
// absolute document position against the viewport instead of its position
// relative to the row's own area, so a top-of-document image only ever got
// prepared once the user scrolled far enough to push it out of view.
#[test]
fn visible_images_start_preparing_without_scrolling() {
    let picker = test_picker();
    let mut document = document_with_images(3);
    let backend = ratatui::backend::TestBackend::new(80, 8);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let mut scroll = 0usize;

    terminal
        .draw(|frame| {
            render(frame, frame.area(), &mut document, &mut scroll, &picker);
        })
        .expect("draw");

    assert_eq!(scroll, 0, "still at the top of the document");
    for (url, image) in &document.images {
        assert!(
            image.pending.is_some() || image.prepared.is_some(),
            "{url} should be preparing or prepared on the first frame"
        );
    }
}

// The symptom being chased: a first-line image shows the fallback icon and
// only turns into the real picture after the user scrolls (any scroll
// forces a redraw). Frame 1 kicks off preparation, the background thread
// finishes, so by frame 2 — with no scrolling at all — the prepared image
// must have been drained in and be ready to draw.
#[test]
fn prepared_image_is_drained_by_the_next_frame_without_scrolling() {
    let picker = test_picker();
    let mut document = document_with_images(1);
    let backend = ratatui::backend::TestBackend::new(80, 8);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let mut scroll = 0usize;

    terminal
        .draw(|frame| {
            render(frame, frame.area(), &mut document, &mut scroll, &picker);
        })
        .expect("first draw");
    assert_eq!(scroll, 0);

    std::thread::sleep(std::time::Duration::from_millis(500));

    terminal
        .draw(|frame| {
            render(frame, frame.area(), &mut document, &mut scroll, &picker);
        })
        .expect("second draw");

    let (url, image) = document
        .images
        .iter()
        .next()
        .expect("document has one image");
    assert!(
        image.prepared.is_some(),
        "{url} should be prepared by the second frame (pending={:?}, prepared={})",
        image.pending.is_some(),
        image.prepared.is_some()
    );
}

// The real description flow: the document is installed with every image
// still Pending, it renders once (fallback icons), and only afterwards do
// the fetched bytes arrive via set_image. Each arrival must kick off
// preparation and become drawable without any scrolling.
#[test]
fn images_loaded_after_first_render_appear_without_scrolling() {
    let picker = test_picker();
    let mut document = Document::new("Title", "![img](https://example.com/0.png)");

    let backend = ratatui::backend::TestBackend::new(80, 8);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let mut scroll = 0usize;

    // frame 1: image still Pending, nothing to prepare
    terminal
        .draw(|frame| {
            render(frame, frame.area(), &mut document, &mut scroll, &picker);
        })
        .expect("first draw");
    assert_eq!(scroll, 0);
    assert!(document.images["https://example.com/0.png"].pending.is_none());

    // the fetch lands
    let decoded = decode_image(&png_bytes(48, 32)).expect("png decodes");
    document.set_image("https://example.com/0.png", Ok(decoded));

    // frame 2: kicks off preparation
    terminal
        .draw(|frame| {
            render(frame, frame.area(), &mut document, &mut scroll, &picker);
        })
        .expect("second draw");

    std::thread::sleep(std::time::Duration::from_millis(500));

    // frame 3: no scrolling anywhere, image should be drawable
    terminal
        .draw(|frame| {
            render(frame, frame.area(), &mut document, &mut scroll, &picker);
        })
        .expect("third draw");

    let image = &document.images["https://example.com/0.png"];
    assert_eq!(scroll, 0, "never scrolled");
    assert!(
        image.prepared.is_some(),
        "late-arriving image should be prepared without scrolling (pending={:?})",
        image.pending
    );
}
