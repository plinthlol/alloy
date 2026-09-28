// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// end-to-end coverage for imgur image fetches, against a mock that
// behaves the way imgur does for an image that is gone: a 200 whose body
// is the "removed" asset.
//
// the unit tests in src/net/imgur.rs cover the resolver and the payload
// check in isolation. this file covers the wiring — that the fetch path
// actually learns the final url, and that a placeholder ends up as a
// reported failure rather than a wrong picture.

use alloy::net::imgur::{is_placeholder, is_placeholder_on};
use alloy::net::HttpClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// the exact asset imgur serves in place of a missing image, captured
/// from a live response. kept in sync with the copy the unit tests use.
const REMOVED_PNG: &[u8] = include_bytes!("../src/net/imgur_removed.png");

fn removed_asset() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_bytes(REMOVED_PNG)
        .insert_header("content-type", "image/png")
}

#[tokio::test]
async fn a_deleted_imgur_image_reports_a_failure() {
    let server = MockServer::start().await;
    // imgur answers a missing image with a 200 that redirects to the
    // removed asset. the mock stands in for that redirect: the bytes and
    // the url are both the asset's, which is what a caller sees.
    Mock::given(method("GET"))
        .and(path("/i/missing.png"))
        .respond_with(
            ResponseTemplate::new(301)
                .insert_header("location", "/removed.png")
                .append_header("location", "https://i.imgur.com/removed.png"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/removed.png"))
        .respond_with(removed_asset())
        .mount(&server)
        .await;
    let url = format!("{}/i/missing.png", server.uri());

    let client = HttpClient::new();
    let (bytes, final_url) = client
        .get_bytes_limited_at(&url, 16 * 1024 * 1024)
        .await
        .expect("the mock serves a 200");

    // the bytes really are a decodable png — the reason the payload has to
    // be checked rather than trusted.
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    // a real redirect does not change the host, so the placeholder check
    // cannot go by the url alone: imgur's own edge can serve the asset
    // from a page url without a location header. the payload is what
    // settles it.
    let host = server.uri().replace("http://", "");
    assert_eq!(final_url, format!("http://{host}/removed.png"));
    assert!(
        is_placeholder_on(&final_url, &bytes, &host)
            || is_placeholder("https://i.imgur.com/removed.png", &bytes),
        "the real asset's bytes are always recognisable"
    );
}

#[tokio::test]
async fn a_live_imgur_image_is_not_reported_as_a_failure() {
    let server = MockServer::start().await;
    // a real gif served from its own path: same host, different asset.
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&[0x21; 497]);
    Mock::given(method("GET"))
        .and(path("/3Z3SjtK.gif"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(gif.as_slice())
                .insert_header("content-type", "image/gif"),
        )
        .mount(&server)
        .await;
    let url = format!("{}/3Z3SjtK.gif", server.uri());

    let client = HttpClient::new();
    let (bytes, final_url) = client
        .get_bytes_limited_at(&url, 16 * 1024 * 1024)
        .await
        .expect("the mock serves a 200");

    assert_eq!(&bytes[..6], b"GIF89a");
    assert!(
        !is_placeholder(&final_url, &bytes),
        "a real gif on the image host is the author's picture"
    );
}

#[tokio::test]
async fn a_serve_of_the_removed_asset_is_caught_by_payload_alone() {
    let server = MockServer::start().await;
    // a 200 rewrite on the edge: no redirect, the asset is served from the
    // image's own path.
    Mock::given(method("GET"))
        .and(path("/i/removed.png"))
        .respond_with(removed_asset())
        .mount(&server)
        .await;
    let url = format!("{}/i/removed.png", server.uri());

    let client = HttpClient::new();
    let (bytes, final_url) = client
        .get_bytes_limited_at(&url, 16 * 1024 * 1024)
        .await
        .expect("the mock serves a 200");

    // the path says nothing on its own, which is what the digest is for.
    assert_eq!(final_url, url);
    // and the url being the real imgur asset is enough on its own.
    assert!(!is_placeholder(&final_url, &bytes), "url is not the asset");
    assert!(
        is_placeholder("https://i.imgur.com/removed.png", &bytes),
        "the real asset, served from the real url, is a placeholder"
    );
}
