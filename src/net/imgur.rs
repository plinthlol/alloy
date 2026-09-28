// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// imgur url resolution for project images.
//
// mod descriptions link images in three shapes, and only one of them is
// directly fetchable:
//
//   https://i.imgur.com/abc123.png   already a raw image — fetch as-is
//   https://imgur.com/abc123        a single image's *page* — the bytes
//                                   live on i.imgur.com, so this needs a
//                                   redirect
//   https://imgur.com/gallery/abc   an album/a page with no single image
//
// `resolve_image_url` rewrites the latter two into an i.imgur.com URL so
// the normal fetch path decodes them. rewriting is all that's needed for
// the single-image page form: imgur serves it as a redirect to the CDN.
//
// NOTE: imgur answers image-host requests for deleted/blocked images with
// a 200 that redirects to its "removed" asset. that is a valid PNG and
// would decode happily into a wrong picture, so the fetch path checks
// `is_placeholder` against the response's final url and reports a failure
// instead. the asset itself is committed alongside this module so the
// check is made against the bytes imgur really serves, not a guess.
//
// the rewrite for the page form is verified end to end: imgur redirects
// `imgur.com/<id>.<ext>` to `i.imgur.com/<id>.<ext>`, which is exactly
// the url this module builds.

const IMGUR_PAGE_HOST: &str = "imgur.com";
const IMGUR_IMAGE_HOST: &str = "i.imgur.com";

// imgur's "this image is gone" asset. the cdn serves this file name in
// place of any image it will not hand out.
const REMOVED_IMAGE: &str = "removed.png";

/// rewrites an imgur page url into a directly fetchable image url, when
/// the url is one this module knows how to handle. anything else — other
/// hosts, or a page whose image is chosen by the server — is returned
/// unchanged so the caller can fetch it as-is.
pub fn resolve_image_url(url: &str) -> String {
    let Some(parts) = split_url(url) else {
        return url.to_string();
    };
    if parts.host != IMGUR_PAGE_HOST {
        return url.to_string();
    }
    // `imgur.com/<id>` and `imgur.com/<id>.<ext>` both name a single image.
    // an explicit extension is kept (a `.gif` link should stay animated);
    // the bare form gets `.png`, which is what the CDN redirects a bare id
    // to anyway.
    match parts.path_segments.as_slice() {
        [segment] if is_image_id(segment) => {
            let (id, ext) = split_extension(segment);
            let ext = ext.unwrap_or("png");
            format!("https://{IMGUR_IMAGE_HOST}/{id}.{ext}")
        }
        // album/gallery/a/t/... have no single image to point at.
        _ => url.to_string(),
    }
}

struct UrlParts {
    host: String,
    path_segments: Vec<String>,
}

fn split_url(url: &str) -> Option<UrlParts> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority
        .split_once(':')
        .map_or(authority, |(host, _)| host)
        .to_ascii_lowercase();
    // `?` and `#` start the query and fragment, which are not path
    // segments. cut them off before splitting so `a.png?v=1` is one
    // segment. `#` goes first so a fragment cannot hide a query.
    let without_fragment = path.split_once('#').map_or(path, |(before, _)| before);
    let path = without_fragment
        .split_once('?')
        .map_or(without_fragment, |(before, _)| before);
    let path_segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    Some(UrlParts {
        host,
        path_segments,
    })
}

// imgur ids are 5 or 7 characters of [A-Za-z0-9]. the 7-char form is the
// old base-62 style; both still appear in live descriptions.
fn is_image_id(segment: &str) -> bool {
    let (id, _) = split_extension(segment);
    matches!(id.len(), 5 | 7) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

// splits a path segment into its id and a lowercase extension, so
// `wTn2Bbf.gif` and `wTn2Bbf` both validate as the same image.
fn split_extension(segment: &str) -> (&str, Option<&str>) {
    segment
        .rsplit_once('.')
        .filter(|(id, ext)| !id.is_empty() && !ext.is_empty() && !ext.contains('/'))
        .map_or((segment, None), |(id, ext)| (id, Some(ext)))
}

/// whether a fetch of an imgur image actually got the author's picture.
///
/// imgur answers a deleted or blocked image with a 200 that redirects to
/// its "removed" asset. two independent tells are checked:
///
/// - the response landed on that asset, which is what the redirect looks
///   like to the http client;
/// - the body is that asset's exact bytes, which catches a serve that
///   redirects without a location header (a 200 rewrite on the edge), and
///   only for imgur — `resolve_image_url` reports whether the url was
///   actually an imgur one.
///
/// returns whether this is a placeholder. a non-imgur url is never a
/// placeholder, whatever it redirects to.
pub fn is_placeholder(final_url: &str, bytes: &[u8]) -> bool {
    is_placeholder_on(final_url, bytes, IMGUR_IMAGE_HOST)
}

/// [`is_placeholder`], with the image host as a parameter so the check can
/// be exercised against a stand-in host.
pub fn is_placeholder_on(final_url: &str, bytes: &[u8], image_host: &str) -> bool {
    if !is_asset_at(final_url, image_host, REMOVED_IMAGE) {
        return false;
    }
    // the real file is 503 bytes. the cap keeps a genuine (if tiny) gif
    // from being misread as one, and bounds the hash to a fixed cost.
    bytes.len() == 503 && md5_of(bytes) == REMOVED_IMAGE_MD5
}

fn md5_of(bytes: &[u8]) -> [u8; 16] {
    use md5::{Digest, Md5};

    let mut hasher = Md5::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// whether `url` is `asset` served from `host`, host-matched
/// case-insensitively with any query and fragment ignored.
fn is_asset_at(url: &str, host: &str, asset: &str) -> bool {
    let Some(parts) = split_url(url) else {
        return false;
    };
    parts.host == host.to_ascii_lowercase()
        && parts.path_segments.as_slice() == [asset.to_ascii_lowercase()]
}

/// md5 of the asset imgur actually served for a missing image, captured
/// from a live `i.imgur.com` response. matching the digest rather than the
/// contents keeps the constant small and ignores nothing that matters —
/// the same file always decodes to the same bytes.
const REMOVED_IMAGE_MD5: [u8; 16] = [
    0xd8, 0x35, 0x88, 0x43, 0x73, 0xf4, 0xd6, 0xc8, 0xf2, 0x47, 0x42, 0xce, 0xab, 0xe7, 0x49, 0x46,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_url_rewrites_to_the_image_host() {
        assert_eq!(
            resolve_image_url("https://imgur.com/wTn2Bbf"),
            "https://i.imgur.com/wTn2Bbf.png"
        );
    }

    #[test]
    fn page_url_with_an_extension_keeps_that_extension() {
        // a .gif page link should stay animated after the rewrite
        assert_eq!(
            resolve_image_url("https://imgur.com/wTn2Bbf.gif"),
            "https://i.imgur.com/wTn2Bbf.gif"
        );
        assert_eq!(
            resolve_image_url("https://imgur.com/wTn2Bbf.jpg"),
            "https://i.imgur.com/wTn2Bbf.jpg"
        );
    }

    #[test]
    fn direct_image_urls_are_left_alone() {
        for url in [
            "https://i.imgur.com/wTn2Bbf.png",
            "https://i.imgur.com/3Z3SjtK.gif",
        ] {
            assert_eq!(resolve_image_url(url), url);
        }
    }

    #[test]
    fn album_and_gallery_urls_are_left_alone() {
        for url in [
            "https://imgur.com/gallery/abc123",
            "https://imgur.com/a/xyz789",
            "https://imgur.com/t/cats/abc123",
        ] {
            assert_eq!(resolve_image_url(url), url);
        }
    }

    #[test]
    fn other_hosts_are_left_alone() {
        for url in [
            "https://cdn.modrinth.com/data/abc/def.png",
            "https://example.com/imgur.com/x",
        ] {
            assert_eq!(resolve_image_url(url), url);
        }
    }

    #[test]
    fn host_and_path_are_matched_case_insensitively() {
        assert_eq!(
            resolve_image_url("https://Imgur.com/wTn2Bbf"),
            "https://i.imgur.com/wTn2Bbf.png"
        );
    }

    #[test]
    fn a_bare_domain_url_is_left_alone() {
        assert_eq!(resolve_image_url("https://imgur.com/"), "https://imgur.com/");
    }

    #[test]
    fn a_placeholder_response_is_recognized() {
        // the real bytes imgur serves in place of a missing image: a
        // 503-byte png. its tail is the png iend chunk, and it carries no
        // text chunks at all, so it is matched by digest.
        let placeholder = real_removed_png();
        assert!(
            is_placeholder("https://i.imgur.com/removed.png", &placeholder),
            "the real payload, served from the real url, is a placeholder"
        );
    }

    #[test]
    fn a_normal_image_is_not_a_placeholder() {
        // a real gif on the imgur host is an author's picture
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0u8; 503 - 6]);
        assert!(!is_placeholder("https://i.imgur.com/3Z3SjtK.gif", &gif));
        // the real placeholder bytes, but on a url that is not the asset
        assert!(!is_placeholder(
            "https://i.imgur.com/3Z3SjtK.png",
            &real_removed_png()
        ));
        // the placeholder url, but different bytes
        assert!(!is_placeholder(
            "https://i.imgur.com/removed.png",
            b"GIF89a this is a different image entirely"
        ));
    }

    #[test]
    fn only_imgur_urls_can_be_placeholders() {
        // a non-imgur host redirecting to a same-named file is fine
        assert!(!is_placeholder(
            "https://example.com/removed.png",
            &real_removed_png()
        ));
        // ... and a non-imgur host's own bytes are never a placeholder
        assert!(!is_placeholder("https://example.com/photo.png", b""));
    }

    #[test]
    fn placeholder_urls_are_matched_case_insensitively_and_ignore_the_query() {
        let placeholder = real_removed_png();
        for url in [
            "https://i.imgur.com/removed.png",
            "https://I.Imgur.com/removed.png",
            "https://i.imgur.com/removed.png?v=1",
            "https://i.imgur.com/removed.png#frag",
        ] {
            assert!(is_placeholder(url, &placeholder), "{url}");
        }
        // a path that merely starts with the asset name is not the asset
        assert!(!is_placeholder(
            "https://i.imgur.com/removed.png/other",
            &placeholder
        ));
    }

    /// the exact 503-byte png imgur serves for a deleted or blocked image.
    fn real_removed_png() -> Vec<u8> {
        const PAYLOAD: &[u8] = include_bytes!("imgur_removed.png");
        assert_eq!(PAYLOAD.len(), 503, "the captured asset's size");
        assert_eq!(md5_of(PAYLOAD), REMOVED_IMAGE_MD5);
        PAYLOAD.to_vec()
    }
}
