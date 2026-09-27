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
// a 200 that redirects to `i.imgur.com/removed.png`. that is a valid PNG
// and would decode happily into a wrong picture, so the fetch path checks
// `is_placeholder_bytes` on the response and reports a failure instead.

const IMGUR_PAGE_HOST: &str = "imgur.com";
const IMGUR_IMAGE_HOST: &str = "i.imgur.com";
// the 1x1 (or 0x0) "image removed" placeholder imgur redirects to.
const REMOVED_IMAGE: &[u8] = b"removed.png";

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
    let path_segments = path
        .split(['/', '?', '#'])
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

/// whether a decoded response is imgur's "this image is gone" placeholder
/// rather than the picture the author linked.
///
/// the placeholder is served with a 200 and a real content-type, so the
/// only reliable tell is the payload itself: a PNG that is a few hundred
/// bytes and is byte-identical to the well-known removed.png. comparing
/// the tail is enough — the file is small and has no trailing data.
pub fn is_placeholder_bytes(bytes: &[u8]) -> bool {
    bytes.len() <= 4096 && bytes.ends_with(REMOVED_IMAGE)
}

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
        // the real removed.png is a 503-byte PNG; the tail is the part
        // that distinguishes it from a normal image.
        let mut placeholder = b"\x89PNG\r\n\x1a\n".to_vec();
        placeholder.extend_from_slice(&[0u8; 400]);
        placeholder.extend_from_slice(b"IEND\xaeB`\x82removed.png");
        assert!(is_placeholder_bytes(&placeholder));
    }

    #[test]
    fn a_normal_image_is_not_a_placeholder() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0u8; 4000]);
        assert!(!is_placeholder_bytes(&png));
        // a same-sized file that merely mentions the placeholder path
        let mut mentions = b"\x89PNG\r\n\x1a\n".to_vec();
        mentions.extend_from_slice(&[0u8; 400]);
        mentions.extend_from_slice(b"IEND\xaeB`\x82real.png");
        assert!(!is_placeholder_bytes(&mentions));
    }
}
