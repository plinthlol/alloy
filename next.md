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
| 3 — Imgur | Resolver verified end to end; **placeholder guard was broken and is now fixed** |
| 1 — images need scrolling | **Not reproduced.** No fix shipped; two hypotheses disproven |

Test suite: 476 passed, 0 failed, 12 ignored. Clippy: 0 errors, 29
warnings, identical to `HEAD` (all pre-existing).

Building and testing needs a JDK on `PATH` for `build.rs`, and
`NO_COLOR` must be unset — with `NO_COLOR=1` in the environment,
`config::theme::tests::resolve_builtin_theme` fails because the theme
resolves to `no-color`. That is an environment interaction, not a
regression.

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

A second guard runs on every fetched response. Imgur answers a deleted or
blocked image with **HTTP 200** that redirects to its "removed" asset — a
valid 503-byte PNG that `decode_still_or_svg` decodes happily. Without
this check a removed image renders as a wrong picture; with it, the URL
reports a failure and the normal fallback icon shows.

**The first version of this guard was broken — see the section below.**

11 unit tests cover the resolver and the placeholder check, and
`tests/net_imgur.rs` (new) covers the fetch path end to end.

## Imgur: the placeholder guard never worked, and the rewrite is confirmed

The earlier note said Imgur "cannot be validated from this machine". The
placeholder behaviour *can* be validated — this environment's network path
reaches imgur just fine, it simply has no image to serve. The ids used in
the first probe turn out to be deleted, so imgur answers with its
"removed" asset, which is exactly the case the guard exists for.

### The guard was broken

The old check was:

```rust
bytes.len() <= 4096 && bytes.ends_with(b"removed.png")
```

The tail comment claimed `removed.png` is "byte-identical to the
well-known removed.png" and that the file "ends with" that name. It does
not. The real asset's last 16 bytes are:

```
327 u " 262 \0 \0 \0 \0 I E N D 256 B ` 202
```

The string `removed.png` appears **nowhere** in the file — no `tEXt` or
`iTXt` chunk, nothing in the raw bytes. `ends_with` is therefore always
false for the real payload, so every deleted or blocked imgur image
rendered as a picture of imgur's placeholder instead of falling back to
the error icon. The feature's one safety net was inert.

The guard's unit test passed because it built its own fixture ending in
the literal `removed.png` — the fixture asserted the bug rather than
catching it. Confirmed by running the old check against the real payload:

```
real payload,  old guard fires: false
hand-built fixture the old test used, old guard fires: true
```

### What the fix does

`is_placeholder_bytes` is replaced by `is_placeholder(final_url, bytes)`,
which checks two independent tells:

- the response landed on `i.imgur.com/removed.png` (the redirect's shape,
  via a new `HttpClient::get_bytes_limited_at` that returns the final URL);
- the body is the asset's exact bytes, matched by MD5 against the
  committed `src/net/imgur_removed.png` (503 bytes, captured from the live
  response), and only for an imgur host.

`is_placeholder_on` takes the host as a parameter so the payload arm is
testable against a stand-in host.

While adding the query/fragment cases the test exposed a latent bug in
`split_url`: chaining `.map_or(path, …)` shadowed `path`, so the fragment
was cut from the un-trimmed string and `removed.png?v=1` never matched.
`#` is now cut first, into a named binding. This also makes
`resolve_image_url` correct for ids carrying a query string, which it
silently mangled before.

`tests/net_imgur.rs` (new, 3 tests) drives the real `HttpClient` against
wiremock serving the committed asset, covering the redirect case, the
direct-serve case, and a real GIF that must *not* be flagged.

### The rewrite is confirmed

`https://imgur.com/3Z3SjtK.gif` 302-redirects to
`https://i.imgur.com/3Z3SjtK.gif` — byte-for-byte the URL
`resolve_image_url` builds. The page→CDN rewrite is verified end to end,
not just by unit test.

### Still open

- Album URLs (`imgur.com/a/…`, `/gallery/…`) have no single image, so they
  stay unsupported. Supporting one needs the Imgur API (client ID), which
  means a new config field.
- No live Imgur id that still resolves was found, so the happy path
  (a real image rendering in the TUI) has not been seen on screen. One
  manual check from a network serving live imgur bytes would close this.
- The GIF work still has not been watched animating; see below.

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
  `java/AlloyShim.java`). A Temurin 21 tarball with no package manager
  available: `curl -sSL "https://api.adoptium.net/v3/binary/latest/21/ga/linux/x64/jdk/hotspot/normal/eclipse"`
  then untar and put its `bin` on `PATH`.
- `src/net/imgur_removed.png` is a captured third-party asset (imgur's
  "removed" placeholder), committed only so the guard matches real bytes.
  It is not an alloy asset and is not covered by the `assets/` icon
  pipeline.
