// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// scanning and toggling instance content (mods, resource packs, worlds).
// minecraft's ".disabled" suffix convention does most of the heavy lifting.

pub mod installed_meta;
pub mod mods;
pub mod resource_packs;
pub mod worlds;

pub use installed_meta::{content_dir_and_stem, record_many, remove_stems};
pub use mods::scan_one_mod;
pub use mods::{
    ContentEntry, IconCell, fallback_icon, make_icon_pixels_from_image, make_icon_quadrants_from_image,
    scan_mods, toggle_entry,
};
pub use resource_packs::{scan_one_resource_pack, scan_resource_packs};
pub use worlds::{scan_one_world, scan_worlds};

use std::io::Read;

// reads enabled/disabled from the ".disabled" suffix and strips the
// extension to get a clean stem name.
pub(crate) fn parse_enabled_stem(file_name: &str, ext: &str) -> Option<(bool, String)> {
    let disabled_ext = format!("{ext}.disabled");
    if let Some(stem) = file_name.strip_suffix(&disabled_ext) {
        Some((false, stem.to_string()))
    } else {
        file_name
            .strip_suffix(ext)
            .map(|stem| (true, stem.to_string()))
    }
}

// same idea but for directories, which don't have a file extension to strip
pub(crate) fn parse_enabled_stem_dir(file_name: &str) -> (bool, String) {
    if let Some(stem) = file_name.strip_suffix(".disabled") {
        (false, stem.to_string())
    } else {
        (true, file_name.to_string())
    }
}

pub(crate) fn read_icon_from_zip(archive: &mut zip::ZipArchive<std::fs::File>) -> Option<Vec<u8>> {
    let mut entry = archive.by_name("pack.png").ok()?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf).ok()?;
    Some(buf)
}

pub(crate) fn open_zip(path: &std::path::Path) -> Option<zip::ZipArchive<std::fs::File>> {
    let file = std::fs::File::open(path).ok()?;
    zip::ZipArchive::new(file).ok()
}
