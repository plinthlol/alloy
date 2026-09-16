// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// account management panel: list, add (microsoft/offline), delete
// microsoft auth uses the device code flow, so it polls a shared mutex
// for the result while showing the user a code to enter in their browser

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use tui_widget_list::{ListBuilder, ListState as TuiListState, ListView};

use crate::auth::{self, AccountStore, AccountType, AuthResult, DeviceCodeInfo};
use crate::config::theme::{BORDER_STYLE, THEME};
use crate::tui::app::FocusedArea;

use super::styled_title;

#[derive(Default)]
pub enum AddMode {
    #[default]
    None,
    // type picker; `selected` is the highlighted row (0 = Microsoft, 1 = offline)
    ChooseType { selected: usize },
    // offline username entry; `cursor` is a char index into `name`
    OfflineNameInput { name: String, cursor: usize },
    OfflineBlocked,
    DeviceCodeWaiting {
        info: DeviceCodeInfo,
        pending: Arc<Mutex<Option<AuthResult>>>,
    },
}

pub struct AccountState {
    pub store: AccountStore,
    pub list_state: TuiListState,
    pub add_mode: AddMode,
}

impl Default for AccountState {
    fn default() -> Self {
        let store = AccountStore::load();
        let mut list_state = TuiListState::default();
        if !store.accounts.is_empty() {
            list_state.selected = Some(0);
        }
        Self {
            store,
            list_state,
            add_mode: AddMode::None,
        }
    }
}

impl AccountState {
    // true while any add-account popup is open. while one is up every
    // keypress belongs to it — the list's delete hotkey ('d') must stay
    // dormant so usernames can contain the letter d.
    pub fn popup_open(&self) -> bool {
        !matches!(self.add_mode, AddMode::None)
    }

    // polled every tick to see if the background auth thread finished;
    // can't block on it because the TUI needs to keep rendering.
    pub fn drain_auth_result(&mut self) {
        if let AddMode::DeviceCodeWaiting { pending, .. } = &self.add_mode {
            let result = match pending.lock() {
                Ok(mut slot) => slot.take(),
                _ => None,
            };

            if let Some(result) = result {
                match result {
                    AuthResult::Success(account) => {
                        self.store.add(account);
                        self.add_mode = AddMode::None;
                        if self.list_state.selected.is_none() && !self.store.accounts.is_empty() {
                            self.list_state.selected = Some(0);
                        }
                    }
                    AuthResult::Error(e) => {
                        tracing::error!("Microsoft auth failed: {}", e);
                        self.add_mode = AddMode::None;
                    }
                }
            }
        }
    }
}

pub fn handle_key(key_event: &KeyEvent, state: &mut AccountState) -> bool {
    match &state.add_mode {
        AddMode::ChooseType { selected } => {
            let selected = *selected;
            match key_event.code {
                KeyCode::Char('m') | KeyCode::Char('1') => start_microsoft_auth_mode(state),
                KeyCode::Char('o') | KeyCode::Char('2') => choose_offline_mode(state),
                KeyCode::Enter => {
                    if selected == 0 {
                        start_microsoft_auth_mode(state)
                    } else {
                        choose_offline_mode(state)
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    state.add_mode = AddMode::ChooseType {
                        selected: (selected + 1).min(1),
                    };
                    true
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    state.add_mode = AddMode::ChooseType {
                        selected: selected.saturating_sub(1),
                    };
                    true
                }
                KeyCode::Esc => {
                    state.add_mode = AddMode::None;
                    true
                }
                _ => true,
            }
        }
        AddMode::OfflineNameInput { name, cursor } => {
            let name = name.clone();
            let cursor = *cursor;
            match key_event.code {
                KeyCode::Enter => {
                    let trimmed = name.trim().to_string();
                    if !trimmed.is_empty() {
                        if state.store.has_any_account() {
                            let account = auth::create_offline_account(&trimmed);
                            state.store.add(account);
                            if state.list_state.selected.is_none()
                                && !state.store.accounts.is_empty()
                            {
                                state.list_state.selected = Some(0);
                            }
                            state.add_mode = AddMode::None;
                        } else {
                            state.add_mode = AddMode::OfflineBlocked;
                        }
                    } else {
                        state.add_mode = AddMode::None;
                    }
                    true
                }
                KeyCode::Char(c) => {
                    let byte_idx = char_byte_index(&name, cursor);
                    let mut new_name = name;
                    new_name.insert(byte_idx, c);
                    state.add_mode = AddMode::OfflineNameInput {
                        name: new_name,
                        cursor: cursor + 1,
                    };
                    true
                }
                KeyCode::Backspace => {
                    let (new_name, new_cursor) = if cursor > 0 {
                        let byte_idx = char_byte_index(&name, cursor);
                        let prev = char_byte_index(&name, cursor - 1);
                        let mut new_name = name;
                        new_name.drain(prev..byte_idx);
                        (new_name, cursor - 1)
                    } else {
                        (name, cursor)
                    };
                    state.add_mode = AddMode::OfflineNameInput {
                        name: new_name,
                        cursor: new_cursor,
                    };
                    true
                }
                // arrow keys move the text cursor, not the account list —
                // the popup owns the keyboard while it's up
                KeyCode::Left => {
                    state.add_mode = AddMode::OfflineNameInput {
                        name,
                        cursor: cursor.saturating_sub(1),
                    };
                    true
                }
                KeyCode::Right => {
                    let len = name.chars().count();
                    state.add_mode = AddMode::OfflineNameInput {
                        name,
                        cursor: (cursor + 1).min(len),
                    };
                    true
                }
                KeyCode::Home => {
                    state.add_mode = AddMode::OfflineNameInput { name, cursor: 0 };
                    true
                }
                KeyCode::End => {
                    let len = name.chars().count();
                    state.add_mode = AddMode::OfflineNameInput { name, cursor: len };
                    true
                }
                KeyCode::Esc => {
                    state.add_mode = AddMode::None;
                    true
                }
                _ => true,
            }
        }
        AddMode::OfflineBlocked => match key_event.code {
            KeyCode::Enter | KeyCode::Esc => {
                state.add_mode = AddMode::None;
                true
            }
            _ => true,
        },
        AddMode::DeviceCodeWaiting { .. } => match key_event.code {
            KeyCode::Esc => {
                state.add_mode = AddMode::None;
                true
            }
            _ => true,
        },
        AddMode::None => {
            let count = state.store.accounts.len();
            match key_event.code {
                KeyCode::Char('a') => {
                    state.add_mode = AddMode::ChooseType { selected: 0 };
                    true
                }
                KeyCode::Enter => {
                    if let Some(idx) = state.list_state.selected {
                        state.store.set_active(idx);
                    }
                    true
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if count > 0 {
                        let cur = state.list_state.selected.unwrap_or(0);
                        state.list_state.selected = Some((cur + 1).min(count - 1));
                    }
                    true
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    let cur = state.list_state.selected.unwrap_or(0);
                    state.list_state.selected = Some(cur.saturating_sub(1));
                    true
                }
                _ => false,
            }
        }
    }
}

// the device code arrives asynchronously from the auth thread, so it's
// pulled out of a global mutex once ready.
pub fn drain_device_code(state: &mut AccountState) {
    if let AddMode::DeviceCodeWaiting { info, .. } = &mut state.add_mode
        && info.user_code.is_empty()
        && let Ok(mut slot) = auth::DEVICE_CODE_DISPLAY.lock()
        && let Some(dc_info) = slot.take()
    {
        info.user_code = dc_info.user_code;
        info.verification_uri = dc_info.verification_uri;
    }
}

pub fn render(frame: &mut Frame, area: Rect, focused: FocusedArea, state: &mut AccountState) {
    let theme = THEME.as_ref();
    let color = if focused == FocusedArea::Account {
        theme.accent()
    } else {
        theme.border()
    };

    let mut block = Block::default()
        .title(styled_title("Accounts", true))
        .borders(Borders::ALL)
        .border_type(BORDER_STYLE.to_border_type())
        .border_style(Style::default().fg(color));

    if focused == FocusedArea::Account {
        let lines = super::popups::keybind_lines_wrapped(
            &[("⏎", " select"), ("a", " add"), ("d", " del")],
            area.width.saturating_sub(2),
        );
        for line in lines {
            block = block.title_bottom(line);
        }
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.store.accounts.is_empty() {
        frame.render_widget(
            Paragraph::new("No accounts.").style(Style::default().fg(theme.text_dim())),
            inner,
        );
    } else {
        let active = state.store.active_account().map(|a| a.uuid.clone());
        render_account_list(frame, inner, state, focused, active.as_deref());
    }

    match &state.add_mode {
        AddMode::ChooseType { selected } => render_choose_popup(frame, *selected),
        AddMode::OfflineNameInput { name, cursor } => {
            render_offline_popup(frame, name, *cursor)
        }
        AddMode::OfflineBlocked => render_offline_blocked_popup(frame),
        AddMode::DeviceCodeWaiting { info, .. } => render_device_code_popup(frame, info),
        AddMode::None => {}
    }
}

fn start_microsoft_auth_mode(state: &mut AccountState) -> bool {
    let pending = auth::start_microsoft_auth();
    state.add_mode = AddMode::DeviceCodeWaiting {
        info: DeviceCodeInfo {
            user_code: String::new(),
            verification_uri: String::new(),
        },
        pending,
    };
    true
}

fn choose_offline_mode(state: &mut AccountState) -> bool {
    state.add_mode = if state.store.has_any_account() {
        AddMode::OfflineNameInput {
            name: String::new(),
            cursor: 0,
        }
    } else {
        AddMode::OfflineBlocked
    };
    true
}

// byte offset for a char-index cursor (usernames are short; the O(n) scan
// keeps the cursor logic unicode-safe without tracking byte positions)
fn char_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(byte, _)| byte)
        .unwrap_or(s.len())
}

fn render_account_list(
    frame: &mut Frame,
    area: Rect,
    state: &mut AccountState,
    focused: FocusedArea,
    active_uuid: Option<&str>,
) {
    let is_focused = focused == FocusedArea::Account;
    let accounts: Vec<(String, AccountType, bool)> = state
        .store
        .accounts
        .iter()
        .map(|a| {
            (
                a.username.clone(),
                a.account_type.clone(),
                active_uuid == Some(&a.uuid),
            )
        })
        .collect();

    let count = accounts.len();

    let builder = ListBuilder::new(move |context| {
        let theme = THEME.as_ref();
        let (username, acc_type, is_active) = &accounts[context.index];
        let show_selected = is_focused && context.is_selected;

        let bg = if show_selected {
            theme.stripe()
        } else {
            theme.background()
        };

        let active_marker = if *is_active { "\u{25b8} " } else { "  " };

        let style = if show_selected {
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD)
        } else if *is_active {
            Style::default()
                .fg(theme.text())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text())
        };

        let mut spans = vec![
            Span::styled(active_marker, Style::default().fg(theme.success())),
            Span::styled(username.clone(), style),
        ];

        if *acc_type == AccountType::Offline {
            let offline_style = if show_selected {
                Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_dim())
            };
            spans.push(Span::styled(" (Offline)", offline_style));
        }

        let item = ratatui::text::Text::from(Line::from(spans)).style(Style::default().bg(bg));
        (item, 1)
    });

    let list = ListView::new(builder, count);
    frame.render_stateful_widget(list, area, &mut state.list_state);
}

// center a popup of given size within the terminal. nothing fancy
fn popup_area(frame: &Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn render_choose_popup(frame: &mut Frame, selected: usize) {
    use super::popups::base::PopupFrame;
    let theme = THEME.as_ref();
    let area = popup_area(frame, 40, 7);

    let border_color = theme.text_dim();
    let dim_color = theme.text_dim();
    let accent_color = theme.success();
    let text_color = theme.text();

    let options = [("m", "Microsoft Account"), ("o", "Offline Account")];

    PopupFrame {
        title: Line::from(" Add Account ").centered(),
        border_color,
        bg: None,
        keybinds: Some(Line::from(Span::styled(
            " ↑↓: choose, ⏎: confirm, Esc: cancel ",
            Style::default().fg(dim_color),
        ))),
        search_line: None,
        content: Box::new(move |inner, buf| {
            let mut text = vec![Line::from("")];
            for (i, (key, label)) in options.iter().enumerate() {
                let is_selected = i == selected;
                let (marker, style) = if is_selected {
                    (
                        "▸ ",
                        Style::default()
                            .fg(accent_color)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    ("  ", Style::default().fg(dim_color))
                };
                text.push(Line::from(vec![
                    Span::styled(marker, style),
                    Span::styled(
                        format!("[{key}] "),
                        if is_selected {
                            style
                        } else {
                            Style::default().fg(dim_color)
                        },
                    ),
                    Span::styled(
                        label.to_string(),
                        if is_selected {
                            Style::default().fg(text_color).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(text_color)
                        },
                    ),
                ]));
            }
            Paragraph::new(text).render(inner, buf);
        }),
    }
    .render(area, frame.buffer_mut());
}

fn render_offline_popup(frame: &mut Frame, name: &str, cursor: usize) {
    use super::popups::{base::PopupFrame, keybind_line};
    let theme = THEME.as_ref();
    let area = popup_area(frame, 40, 5);
    let name = name.to_string();
    let cursor = cursor.min(name.chars().count());

    let border_color = theme.text_dim();
    let bg_color = theme.surface();
    let dim_color = theme.text_dim();
    let text_color = theme.text();

    PopupFrame {
        title: Line::from(Span::styled(
            " Offline Account ",
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ))
        .centered(),
        border_color,
        bg: Some(bg_color),
        keybinds: Some(keybind_line(&[
            ("←→", " move"),
            ("Enter", " confirm"),
            ("Esc", " cancel"),
        ])),
        search_line: None,
        content: Box::new(move |inner, buf| {
            let line = if name.is_empty() {
                Line::from(vec![
                    Span::styled("Username...", Style::default().fg(dim_color)),
                    Span::styled(
                        "\u{2588}",
                        Style::default()
                            .fg(border_color)
                            .add_modifier(Modifier::SLOW_BLINK),
                    ),
                ])
            } else {
                let byte_idx = char_byte_index(&name, cursor);
                let (before, after) = name.split_at(byte_idx);
                let mut spans = vec![Span::styled(
                    before.to_string(),
                    Style::default().fg(text_color),
                )];
                // block cursor: inverts the char it sits on, or blinks at
                // the end of the input
                match after.chars().next() {
                    Some(ch) => {
                        spans.push(Span::styled(
                            ch.to_string(),
                            Style::default().fg(bg_color).bg(text_color),
                        ));
                        spans.push(Span::styled(
                            after[ch.len_utf8()..].to_string(),
                            Style::default().fg(text_color),
                        ));
                    }
                    None => spans.push(Span::styled(
                        "\u{2588}",
                        Style::default()
                            .fg(border_color)
                            .add_modifier(Modifier::SLOW_BLINK),
                    )),
                }
                Line::from(spans)
            };
            Paragraph::new(line).render(inner, buf);
        }),
    }
    .render(area, frame.buffer_mut());
}

fn render_offline_blocked_popup(frame: &mut Frame) {
    use super::popups::{base::PopupFrame, keybind_line};
    let theme = THEME.as_ref();
    let area = popup_area(frame, 58, 5);

    let border_color = theme.text_dim();
    let bg_color = theme.surface();
    let text_color = theme.text();

    PopupFrame {
        title: Line::from(Span::styled(
            " Offline Account ",
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ))
        .centered(),
        border_color,
        bg: Some(bg_color),
        keybinds: Some(keybind_line(&[("Enter", " close")])),
        search_line: None,
        content: Box::new(move |inner, buf| {
            Paragraph::new("Add a Microsoft account that owns Minecraft first.")
                .style(Style::default().fg(text_color))
                .render(inner, buf);
        }),
    }
    .render(area, frame.buffer_mut());
}

fn render_device_code_popup(frame: &mut Frame, info: &DeviceCodeInfo) {
    use super::popups::{base::PopupFrame, keybind_line};
    let theme = THEME.as_ref();
    let area = popup_area(frame, 50, 8);
    let uri = info.verification_uri.clone();
    let code = info.user_code.clone();

    let border_color = theme.text_dim();
    let bg_color = theme.surface();
    let dim_color = theme.text_dim();
    let accent_color = theme.success();

    PopupFrame {
        title: Line::from(Span::styled(
            " Microsoft Login ",
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ))
        .centered(),
        border_color,
        bg: Some(bg_color),
        keybinds: Some(keybind_line(&[("Esc", " cancel")])),
        search_line: None,
        content: Box::new(move |inner, buf| {
            let text = if code.is_empty() {
                vec![Line::from(Span::styled(
                    "Connecting to Microsoft...",
                    Style::default().fg(dim_color),
                ))]
            } else {
                vec![
                    Line::from(Span::styled(
                        "Open this URL in your browser:",
                        Style::default().fg(dim_color),
                    )),
                    Line::from(Span::styled(
                        uri.as_str(),
                        Style::default()
                            .fg(accent_color)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("Enter code: ", Style::default().fg(dim_color)),
                        Span::styled(
                            code.as_str(),
                            Style::default()
                                .fg(accent_color)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Waiting for authentication...",
                        Style::default().fg(dim_color),
                    )),
                ]
            };
            Paragraph::new(text).render(inner, buf);
        }),
    }
    .render(area, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    // while the offline-name popup is open, a typed 'd' must land in the
    // name (this is what the App-level popup_open() guard routes here —
    // regression test for 'd' opening the delete prompt mid-typing)
    #[test]
    fn offline_name_typing_d_inserts_d() {
        let mut state = AccountState::default();
        state.add_mode = AddMode::OfflineNameInput {
            name: "ab".into(),
            cursor: 2,
        };
        assert!(handle_key(&key(KeyCode::Char('d')), &mut state));
        match &state.add_mode {
            AddMode::OfflineNameInput { name, cursor } => {
                assert_eq!(name, "abd");
                assert_eq!(*cursor, 3);
            }
            _ => panic!("expected OfflineNameInput"),
        }
    }

    #[test]
    fn offline_name_arrows_move_cursor() {
        let mut state = AccountState::default();
        state.add_mode = AddMode::OfflineNameInput {
            name: "abc".into(),
            cursor: 3,
        };
        handle_key(&key(KeyCode::Left), &mut state);
        handle_key(&key(KeyCode::Char('x')), &mut state);
        match &state.add_mode {
            AddMode::OfflineNameInput { name, cursor } => {
                assert_eq!(name, "abxc");
                assert_eq!(*cursor, 3);
            }
            _ => panic!("expected OfflineNameInput"),
        }
        // cursor clamps at both ends
        handle_key(&key(KeyCode::Left), &mut state);
        for _ in 0..10 {
            handle_key(&key(KeyCode::Left), &mut state);
        }
        assert!(matches!(
            state.add_mode,
            AddMode::OfflineNameInput { cursor: 0, .. }
        ));
        handle_key(&key(KeyCode::Right), &mut state);
        for _ in 0..10 {
            handle_key(&key(KeyCode::Right), &mut state);
        }
        assert!(matches!(
            state.add_mode,
            AddMode::OfflineNameInput { cursor: 4, .. }
        ));
    }

    #[test]
    fn offline_name_backspace_deletes_before_cursor() {
        let mut state = AccountState::default();
        state.add_mode = AddMode::OfflineNameInput {
            name: "abc".into(),
            cursor: 2,
        };
        handle_key(&key(KeyCode::Backspace), &mut state);
        match &state.add_mode {
            AddMode::OfflineNameInput { name, cursor } => {
                assert_eq!(name, "ac");
                assert_eq!(*cursor, 1);
            }
            _ => panic!("expected OfflineNameInput"),
        }
        // backspace at position 0 is a no-op but stays in the popup
        handle_key(&key(KeyCode::Backspace), &mut state);
        handle_key(&key(KeyCode::Backspace), &mut state);
        assert!(matches!(
            state.add_mode,
            AddMode::OfflineNameInput { cursor: 0, .. }
        ));
    }

    #[test]
    fn choose_type_arrows_move_selection() {
        let mut state = AccountState::default();
        state.add_mode = AddMode::ChooseType { selected: 0 };
        handle_key(&key(KeyCode::Down), &mut state);
        assert!(matches!(
            state.add_mode,
            AddMode::ChooseType { selected: 1 }
        ));
        handle_key(&key(KeyCode::Down), &mut state);
        assert!(matches!(
            state.add_mode,
            AddMode::ChooseType { selected: 1 }
        ));
        handle_key(&key(KeyCode::Up), &mut state);
        assert!(matches!(
            state.add_mode,
            AddMode::ChooseType { selected: 0 }
        ));
    }

    #[test]
    fn popup_open_tracks_add_mode() {
        let mut state = AccountState::default();
        assert!(!state.popup_open());
        state.add_mode = AddMode::ChooseType { selected: 0 };
        assert!(state.popup_open());
        state.add_mode = AddMode::OfflineNameInput {
            name: "d".into(),
            cursor: 1,
        };
        assert!(state.popup_open());
    }

    // the account list itself must accept arrow keys for navigation
    // (j/k are the documented bindings; Up/Down ride along)
    #[test]
    fn list_arrows_clamp_to_bounds() {
        let mut state = AccountState::default();
        let count = state.store.accounts.len();
        if count == 0 {
            assert!(handle_key(&key(KeyCode::Down), &mut state));
            assert_eq!(state.list_state.selected, None);
            return;
        }
        state.list_state.selected = Some(0);
        for _ in 0..count + 3 {
            handle_key(&key(KeyCode::Down), &mut state);
        }
        assert_eq!(state.list_state.selected, Some(count - 1));
        for _ in 0..count + 3 {
            handle_key(&key(KeyCode::Up), &mut state);
        }
        assert_eq!(state.list_state.selected, Some(0));
    }
}
