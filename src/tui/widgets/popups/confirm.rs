// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// "are you sure?" popup for destructive actions. uses global state so the
// confirmation target persists across render frames.

use std::sync::LazyLock;
use std::sync::Mutex;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use crate::config::theme::THEME;
use crate::tui::app::FocusedArea;

static CONFIRM_STATE: LazyLock<Mutex<ConfirmState>> =
    LazyLock::new(|| Mutex::new(ConfirmState::default()));

#[derive(Debug, Default)]
struct ConfirmState {
    target: Option<ConfirmTarget>,
}

#[derive(Debug, Clone)]
pub enum ConfirmTarget {
    Instance {
        name: String,
    },
    Account {
        username: String,
        index: usize,
    },
    Content {
        name: String,
        path: std::path::PathBuf,
    },
    /// stop a running instance. `origin` is the focus area to restore on
    /// confirm or cancel — the Esc kill binding fires from Instances or
    /// Content, and the popup must hand focus back to where it came from.
    Kill {
        name: String,
        origin: FocusedArea,
    },
}

impl ConfirmTarget {
    fn title(&self) -> String {
        match self {
            ConfirmTarget::Kill { name, .. } => format!(" Kill '{name}' "),
            _ => format!(" Delete '{}' ", self.name()),
        }
    }

    fn body(&self) -> &'static str {
        match self {
            ConfirmTarget::Instance { .. } => "This will permanently remove the instance",
            ConfirmTarget::Account { .. } => "This will permanently remove this account",
            ConfirmTarget::Content { .. } => "This will permanently remove the selected item",
            ConfirmTarget::Kill { .. } => "This will stop the running game",
        }
    }

    pub fn name(&self) -> &str {
        match self {
            ConfirmTarget::Instance { name } => name,
            ConfirmTarget::Account { username, .. } => username,
            ConfirmTarget::Content { name, .. } => name,
            ConfirmTarget::Kill { name, .. } => name,
        }
    }
}

pub fn set_pending(target: ConfirmTarget) {
    match CONFIRM_STATE.lock() {
        Ok(mut s) => {
            s.target = Some(target);
        }
        Err(e) => {
            tracing::error!("Confirm popup state lock poisoned: {}", e);
        }
    }
}

pub fn set_pending_delete(name: impl Into<String>) {
    set_pending(ConfirmTarget::Instance { name: name.into() });
}

pub fn set_pending_instance_delete(name: impl Into<String>) {
    set_pending_delete(name);
}

pub fn set_pending_content_delete(name: impl Into<String>, path: impl Into<std::path::PathBuf>) {
    set_pending(ConfirmTarget::Content {
        name: name.into(),
        path: path.into(),
    });
}

pub fn set_pending_kill(name: impl Into<String>, origin: FocusedArea) {
    set_pending(ConfirmTarget::Kill {
        name: name.into(),
        origin,
    });
}

pub fn pending_target() -> Option<ConfirmTarget> {
    match CONFIRM_STATE.lock() {
        Ok(s) => s.target.clone(),
        Err(_) => None,
    }
}

pub fn clear_pending() {
    match CONFIRM_STATE.lock() {
        Ok(mut s) => {
            s.target = None;
        }
        Err(e) => {
            tracing::error!("Confirm popup state lock poisoned: {}", e);
        }
    }
}

pub struct ConfirmPopup {
    title: String,
    body: &'static str,
    keybinds: &'static [(&'static str, &'static str)],
}

impl ConfirmPopup {
    pub fn new(
        title: impl Into<String>,
        body: &'static str,
        keybinds: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self {
            title: title.into(),
            body,
            keybinds,
        }
    }

    pub fn for_target(target: &ConfirmTarget) -> Self {
        match target {
            // ⏎ kills, b/Esc backs out — the whole point of the popup is
            // a cheap, discoverable confirmation, not a quiz.
            ConfirmTarget::Kill { .. } => {
                Self::new(target.title(), target.body(), &[("Enter", " kill"), ("b/Esc", " back")])
            }
            _ => Self::new(target.title(), target.body(), &[("Enter", " confirm")]),
        }
    }
}

impl Widget for ConfirmPopup {
    fn render(self, area: Rect, buf: &mut Buffer) {
        use super::{base::PopupFrame, keybind_line};

        let theme = THEME.as_ref();
        let title = Line::from(vec![Span::styled(
            self.title,
            Style::default()
                .fg(theme.text_dim())
                .add_modifier(Modifier::BOLD),
        )]);
        let kb = keybind_line(self.keybinds);

        let border_color = theme.text_dim();
        let bg_color = theme.surface();
        let text_color = theme.text();
        let popup = PopupFrame {
            title,
            border_color,
            bg: Some(bg_color),
            keybinds: Some(kb),
            search_line: None,
            content: Box::new(move |inner, buf| {
                Paragraph::new(self.body)
                    .style(Style::default().fg(text_color))
                    .render(inner, buf);
            }),
        };

        popup.render(area, buf);
    }
}

pub fn confirm_popup_area(frame_area: Rect, target: &ConfirmTarget) -> Rect {
    use super::word_wrap_size;
    use ratatui::layout::Constraint;
    const MAX_W: usize = 48;
    let title_w = target.name().len() + 12;
    let (body_w, _) = word_wrap_size(target.body(), MAX_W);
    let inner_w = title_w.max(body_w).min(MAX_W);
    let (_, lines) = word_wrap_size(target.body(), inner_w);
    let popup_w = ((inner_w + 2) as u16).min(frame_area.width.saturating_sub(4));
    let popup_h = ((lines + 2) as u16).min(frame_area.height.saturating_sub(4));
    frame_area.centered(Constraint::Length(popup_w), Constraint::Length(popup_h))
}
