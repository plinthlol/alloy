// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// live end-to-end check of the description gallery: fetch a real project,
// walk the same url set description.rs builds, and decode every image.
//
// this exists because the unit tests only ever fed the decoder PNG and GIF
// fixtures. Modrinth's gallery thumbnails are static `_350.webp` files, and
// a static webp yields zero frames to the animation decoder — every cell in
// the grid came back as a failure and sat on "loading..." forever. a
// synthetic fixture cannot catch that; the real bytes can.

use alloy::net::{HttpClient, MAX_PROVIDER_ASSET_BYTES};
use alloy::tui::widgets::markdown;

// a project with a populated gallery: Sodium.
const PROJECT_ID: &str = "AANobbMI";

async fn fetch_and_decode(client: &HttpClient, url: &str) -> Result<usize, String> {
    let fetch_url = alloy::net::imgur::resolve_image_url(url);
    let (bytes, final_url) = client
        .get_bytes_limited_at(&fetch_url, MAX_PROVIDER_ASSET_BYTES)
        .await
        .map_err(|e| e.to_string())?;
    if alloy::net::imgur::is_placeholder(&final_url, &bytes) {
        return Err("imgur placeholder".to_owned());
    }
    let animation = tokio::task::spawn_blocking(move || markdown::decode_image_frames(&bytes))
        .await
        .map_err(|e| e.to_string())??;
    Ok(animation.frames.len())
}

#[tokio::test]
#[ignore = "hits live Modrinth API"]
async fn every_modrinth_gallery_thumbnail_decodes() {
    let client = HttpClient::shared();
    let project = alloy::net::modrinth::get_project(&client, PROJECT_ID)
        .await
        .expect("project fetches");
    assert!(
        !project.gallery.is_empty(),
        "{PROJECT_ID} should still have screenshots for this test to mean anything"
    );

    for image in &project.gallery {
        let frames = fetch_and_decode(&client, &image.url).await.unwrap_or_else(|error| {
            panic!("gallery thumbnail {} failed to decode: {error}", image.url)
        });
        assert_eq!(frames, 1, "{} is a still, not an animation", image.url);
    }
}

#[tokio::test]
#[ignore = "hits live image CDNs"]
async fn a_curseforge_style_description_image_decodes() {
    // CurseForge has no gallery endpoint, so its grid is built from the
    // images embedded in the HTML description — which for real projects
    // (JEI, for one) are imgur urls. no API key needed for the part that
    // was actually broken: turning those bytes into a picture.
    let client = HttpClient::shared();
    let frames = fetch_and_decode(&client, "https://i.imgur.com/YTuxxOL.png")
        .await
        .expect("the imgur image embedded in JEI's CurseForge description decodes");
    assert_eq!(frames, 1);
}
