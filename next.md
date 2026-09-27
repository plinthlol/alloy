# next.md

Handoff notes for the mod-description work: what changed, what was verified,
and what is still open.

## The request

Four things were asked for:

1. Images in a mod description only appeared after scrolling, even when the
   image was on the first line.
2. GIF support in the gallery.
3. Imgur support in the gallery.
4. Gallery images were being repeated inside the description body — remove
   the `## Gallery` section.

Confirmed scope with the user: remove the whole `## Gallery` section, and
cover both GIF and Imgur.

## Status

| Item | State |
|---|---|
| 4 — `## Gallery` removed from the description body | Done, verified in the TUI |
| 2 — GIF playback | Done, unit-tested, not yet seen animating on screen |
| 3 — Imgur | Implemented, but cannot be validated from this machine (see below) |
| 1 — images need scrolling | **Not reproduced.** No fix shipped; two hypotheses disproven |

Test suite: 471 passed, 0 failed, 12 ignored. Clippy: 0 errors, 20
warnings, identical to clean `HEAD` (all pre-existing).

## What changed

### `## Gallery` removed from the description body

`src/tui/widgets/popups/description.rs`

`append_gallery(body, gallery) -> String` used to splice
`![title](raw_url)` for every gallery image into the end of the markdown
body. That is why pressing `G` on a project showed the gallery images a
second time as part of the prose.

It is replaced by `gallery_items(&[GalleryImage]) -> Vec<GalleryItem>`,
which only maps the API response into grid entries and leaves the body
alone. Items are sorted by `image.ordering` as before.

Verified in the running TUI: the description now ends with the author's
own content ("Links", "Installation", "FAQ") and the `g` grid still shows
`Modrinth gallery · 4 images`.

### GIF (and animated WebP) playback

`src/tui/widgets/markdown.rs`

The decoder produced a single `DynamicImage` before, so an animated GIF
showed frame 0 forever. Now:

- `decode_image_frames(bytes) -> Result<DecodedAnimation, String>` dispatches
  on `image::guess_format`; `Gif` and `WebP` go to `decode_animated`,
  everything else takes the old still/SVG path and yields exactly one
  frame. `decode_image` is kept as a thin wrapper for callers that only
  want the still.
- `decode_animated` uses `image::AnimationDecoder` with concrete
  `GifDecoder` / `WebPDecoder`, composites each frame's sub-rectangle onto
  a full-size canvas at its `(left, top)` offset, and reads the per-frame
  delay. `GifEncoder`/`WebPDecoder` iterator is bound as `decoder_frames`
  to avoid shadowing the local `frames` vec.
- Guards: `MAX_ANIMATION_FRAMES = 120`, `MAX_ANIMATION_FRAME_BYTES = 8 MiB`
  per canvas, and delays are clamped up to `MIN_FRAME_DELAY_MS = 100` (a
  browser-compatible floor) so a 1 ms-delay GIF can't spin the redraw loop.
- `DocumentImage` gained an `animation: Option<Animation>` field, and
  `ImageRenderKey` a `frame: usize` (0 for stills, so still render keys are
  byte-for-byte unchanged). `current_frame` returns a *cloned* `DynamicImage`
  — returning a borrow fought with the later writes to `pending`/`prepared`.
- The `Animation` clock wraps on `loop_duration()` rather than accumulating
  per-frame drift, and `next_deadline` re-bases on each frame so a long
  session can't slide.
- `Document::next_animation_deadline()` reports the soonest frame change.

`src/tui/widgets/popups/description.rs`

- The fetch path calls `decode_image_frames` and installs the result with
  `Document::set_animated_image`, and calls `request_redraw()` after each
  completed fetch.
- `render_content` reads `next_animation_deadline()` and calls
  `request_redraw()` when a frame is due within `FRAME_EAGER_WINDOW`
  (40 ms — comfortably past the event loop's 16 ms poll). Without this a GIF
  sits on frame 0 until the user happens to press a key.
- The process-wide image cache still stores only the *still*
  (`animation.first`), since the grid and preview build from that; frames
  live in the `Document` and die with the popup.

### Imgur

`src/net/imgur.rs` (new) + wiring in `src/net/mod.rs` and
`src/tui/widgets/popups/description.rs`

`resolve_image_url(url)` rewrites the page form to a fetchable image URL:

| URL | Result |
|---|---|
| `https://i.imgur.com/abc.png` | unchanged (already a raw image) |
| `https://imgur.com/abc` | `https://i.imgur.com/abc.png` |
| `https://imgur.com/abc.gif` | `https://i.imgur.com/abc.gif` (extension preserved so GIFs stay animated) |
| `https://imgur.com/gallery/abc` | unchanged — no single image to point at |

Image IDs are validated as 5 or 7 alphanumeric characters, host matching is
case-insensitive, and userinfo/port/query/fragment are stripped before
matching.

A second guard, `is_placeholder_bytes()`, runs on every fetched response.
Imgur answers a deleted or blocked image with **HTTP 200** that redirects to
`i.imgur.com/removed.png` — a valid 503-byte PNG that `decode_still_or_svg`
decodes happily. Without this check a removed image would render as a wrong
picture; with it, the URL reports a failure and the normal fallback icon
shows.

9 unit tests cover the resolver and the placeholder check.

## Open: Imgur cannot be validated here

A live probe from this machine, using alloy's exact user agent
(`alloy/3.80.0 (Minecraft Launcher)`) and reqwest defaults:

| URL | status | final URL | bytes |
|---|---|---|---|
| `i.imgur.com/wTn2Bbf.png` | 200 | `i.imgur.com/removed.png` | 503 |
| `i.imgur.com/3Z3SjtK.gif` | 200 | `i.imgur.com/removed.png` | 503 |
| `imgur.com/wTn2Bbf.png` | 200 | `i.imgur.com/removed.png` | 503 |
| `imgur.com/gallery/abc123` | 200 | unchanged | 5478 (HTML) |

A Chrome user agent gets the identical placeholder, and album HTML carries
no `og:image` to scrape. So this environment serves every Imgur request a
"removed" picture regardless of headers.

Consequences:

- The resolver's rewrite is verified by unit tests only. It needs one manual
  check from a network that serves real Imgur bytes.
- The placeholder guard is the reason the feature is safe to ship at all.
  Without it, the 503-byte PNG would decode and display as if it were the
  author's picture.
- Album URLs genuinely have no single image, so they stay unsupported. If
  one of those needs to work it requires the Imgur API (client ID), which
  means a new config field.
- No real Imgur URLs appeared in the first 20 Modrinth search hits, so there
  is no convenient end-to-end fixture yet.

## Open: the scroll bug was not reproduced

Reported as: images in a mod description only load after scrolling, even when
the image is on the first line.

What was done:

- Built a real Fabric instance, opened the Modrinth browse popup, and opened
  **Iris Shaders** (whose body begins with a first-line image). The image
  rendered immediately, no scrolling required.
- Three unit tests were added as regression cover for the "late-arriving
  image must render without scrolling" path.
- Render-path instrumentation logged
  `TRACE row index=0 doc_y=0 h=2 viewport=0..8 in_view=true` — a first-line
  image is correctly inside the viewport, so the pre-warm and in-view math
  are right. `markdown.rs` was then restored from backup; `git diff` verified
  clean, so no instrumentation leaked into the commit.

Two hypotheses were tested and disproven:

1. Off-screen pre-warm viewport math — disproven by the trace above.
2. Late-arriving images never being marked pending — disproven; the
   `ImageLoad::Pending` path is correct and the regression tests pass.

**The one hypothesis still untested: it may depend on the terminal's image
protocol.** tmux forces the Halfblocks protocol; Kitty and Sixel take a
different `ImageRenderKey` path, and `render.rs:108` calls
`content.invalidate_image_protocols()` when an overlay closes. That is the
first thing to check.

To reproduce, we need:

- which terminal the user runs, and
- what `image_protocol` is set to in `config.toml`.

Three regression tests that should be re-run once it reproduces are in
`src/tui/tests/widgets/markdown.rs`.

## Other notes

- `src/tui/widgets/content/mod.rs` needed a one-word fix
  (`let mut content = ContentArea::default();`) to compile its own test
  suite. This is pre-existing breakage on `HEAD`, introduced by `10fd21b`
  which added `clear_search(&mut self)`. The test suite did not compile at
  `HEAD` before this branch.
- `src/net/mod.rs` also carries an earlier fix from this session: a
  duplicated `self.gate(url).await` in `HttpClient::get` was halving the
  effective Modrinth/CurseForge rate limit (1 req/s instead of 2 — 6
  requests took 5.01 s rather than 2.00 s).
- CI (`.github/workflows/release-binary.yml`) builds releases but never runs
  `cargo test` or clippy. That is why the broken test on `HEAD` went
  unnoticed. Worth fixing separately.
- `decode_image_renders_svg_with_unavailable_fonts` needs system fonts
  installed (`fontconfig ttf-dejavu`). With zero fonts, SVG description
  text renders invisibly — a real environment dependency, not a test
  artifact.
- Building requires a JDK on `PATH` (`build.rs` compiles
  `java/AlloyShim.java`); this machine needed `jdk21-openjdk` and
  `export PATH="/usr/lib/jvm/java-21-openjdk/bin:$PATH"`.
