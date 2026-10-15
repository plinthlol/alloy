// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// tui entrypoint: sets up the terminal, runs the app, cleans up on exit.

pub mod app;
pub mod error_buffer;
pub mod events;
mod event;
mod input;
pub mod logging;
pub mod progress;
mod render;
pub mod widgets;

use std::sync::atomic::{AtomicBool, Ordering};

static REDRAW_REQUESTED: AtomicBool = AtomicBool::new(true);

pub fn request_redraw() {
    REDRAW_REQUESTED.store(true, Ordering::Release);
}

pub(super) fn take_redraw_request() -> bool {
    REDRAW_REQUESTED.swap(false, Ordering::AcqRel)
}

pub type Tui = ratatui::DefaultTerminal;

pub async fn show() -> color_eyre::Result<()> {
    // restore the terminal before printing a panic — otherwise raw mode +
    // alternate screen stay active and it looks like a freeze.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
        reset_keyboard_protocol();
        ratatui::restore();
        reset_keyboard_protocol();
        default_hook(info);
    }));

    // try_init instead of init: when stdin/stdout isn't a usable TTY (piped
    // launch, IDE-embedded terminal, another instance still holding the tty),
    // raw-mode setup fails with EIO — print a clean message and exit 0-style
    // instead of panicking with a raw Os error.
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(e) => {
            eprintln!("Failed to initialize terminal: {e}");
            eprintln!(
                "alloy needs an interactive terminal — run it directly in a terminal \
                 emulator (not piped or IDE-embedded), and make sure no other \
                 instance is still holding the terminal."
            );
            return Ok(());
        }
    };

    // opt into enhanced keyboard protocol to distinguish key press vs release
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    );

    // figure out the terminal's font cell size for rendering images.
    // falls back to halfblock characters if the terminal doesn't respond
    let mut picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    let detected_protocol = picker.protocol_type();
    // detected_protocol already reflects ratatui-image's capability chain
    // (Kitty → Sixel → iTerm2 → Halfblocks). for any "fancy" config choice
    // we trust that result rather than demanding an exact match — config
    // "kitty" on a Sixel-only terminal should render Sixel, not silently
    // fall to Halfblocks. Halfblocks/Quadrants in config are an explicit
    // opt-out.
    let is_auto_tier = matches!(
        crate::config::SETTINGS.ui.image_protocol,
        crate::config::settings::ImageProtocol::Sixel
            | crate::config::settings::ImageProtocol::Kitty
            | crate::config::settings::ImageProtocol::Iterm2
    );
    let requested_protocol = if is_auto_tier {
        detected_protocol
    } else {
        ratatui_image::picker::ProtocolType::Halfblocks
    };
    picker.set_protocol_type(requested_protocol);

    // persist what actually got detected this run. this runs every launch
    // (not just the very first one) so the Settings screen's "Image
    // Protocol" row always shows the terminal alloy is *actually* drawing
    // to right now, rather than whatever was bundled as the compiled
    // default or detected in some previous, different terminal — that
    // mismatch is exactly what made the row look stuck on "kitty"
    // regardless of what terminal alloy was launched from. an explicit
    // Halfblocks/Quadrants opt-out is left untouched since it isn't
    // something detection should ever overwrite.
    if is_auto_tier {
        persist_detected_protocol(detected_protocol);
    }

    let mut app = app::App::new(picker);
    let result = app.run(&mut terminal).await;

    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags
    );
    reset_keyboard_protocol();

    ratatui::restore();

    // the keyboard-protocol stack is tracked per screen by some terminals
    // (and implemented loosely by others), so clear it again on the main
    // screen. otherwise the shell can be left receiving CSI-u key reports
    // (Enter arrives as an escape sequence and commands never run).
    reset_keyboard_protocol();

    // config.toml is read once at startup (see config::SETTINGS), so the
    // only way to apply settings-screen edits is a fresh process — and only
    // if the user actually saved (Ctrl+S) before closing with Esc.
    if app.restart_requested {
        respawn();
    }

    result
}

// force the kitty keyboard protocol back to its default state: set flags to
// 0, then pop any entries still on the stack (popping an empty stack is a
// no-op). harmless on terminals that don't support the protocol.
fn reset_keyboard_protocol() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[=0;1u\x1b[<9u");
    let _ = out.flush();
}

// writes the just-detected protocol into config.toml when it differs from
// what's saved, so a fresh terminal's capability (kitty -> sixel -> iterm2
// -> halfblocks, whatever from_query_stdio actually found) becomes the new
// persisted value instead of silently diverging from what Settings shows.
// SETTINGS itself is loaded once at startup and stays immutable for this
// process either way — this only affects what the *next* launch reads.
fn persist_detected_protocol(detected: ratatui_image::picker::ProtocolType) {
    let detected_as_config = match detected {
        ratatui_image::picker::ProtocolType::Kitty => crate::config::settings::ImageProtocol::Kitty,
        ratatui_image::picker::ProtocolType::Sixel => crate::config::settings::ImageProtocol::Sixel,
        ratatui_image::picker::ProtocolType::Iterm2 => crate::config::settings::ImageProtocol::Iterm2,
        ratatui_image::picker::ProtocolType::Halfblocks => {
            crate::config::settings::ImageProtocol::Halfblocks
        }
    };

    if crate::config::SETTINGS.ui.image_protocol == detected_as_config {
        return;
    }

    let mut updated = crate::config::SETTINGS.clone();
    updated.ui.image_protocol = detected_as_config;
    if let Err(e) = crate::config::save_config(&updated) {
        tracing::warn!("Failed to persist detected image protocol: {}", e);
    }
}

// re-execs the current binary with the same args, then exits. the terminal
// is already restored above, so the new process starts on a clean one.
fn respawn() {
    let Ok(exe) = std::env::current_exe() else {
        tracing::error!("Couldn't resolve current executable path to restart alloy");
        return;
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Err(e) = std::process::Command::new(exe).args(args).spawn() {
        tracing::error!("Failed to restart alloy: {e}");
    }
}
