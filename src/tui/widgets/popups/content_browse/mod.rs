// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

mod render;
mod state;

pub use render::{popup_rect, render};
pub use state::{
    ContentInstallParams, ContentInstallSource, ContentKind, clear_installed, confirm_installed,
    handle_key, is_open, open, refresh_installed,
};
