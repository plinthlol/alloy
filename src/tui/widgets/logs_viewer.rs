// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// split-pane log viewer: file list on the left, log content on the right.
// supports live log tailing when the instance is running, plus search
// filtering in both the file list and the viewer pane.
// log scanning runs on a background thread to avoid blocking the UI.

use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};
use tui_widget_list::{ListBuilder, ListState as TuiListState, ListView};

use crate::config::theme::{BORDER_STYLE, THEME};
use crate::instance::launch::parser::LogLevel;
use crate::instance::log_files::{LogFileEntry, read_log_file, scan_log_files};

type PendingLogs = Arc<Mutex<Option<(String, Vec<LogFileEntry>)>>>;

pub struct LogsState {
    pub entries: Vec<LogFileEntry>,
    pub list_state: TuiListState,
    pub loaded_for: Option<String>,
    pub loading: bool,
    pub viewer_focused: bool,
    pub viewer_lines: Vec<String>,
    pub viewer_scroll: usize,
    pub viewer_max_scroll: usize,
    pub scrollbar_state: ScrollbarState,
    pub viewer_scrollbar_state: ScrollbarState,
    pub search: super::search::SearchState,
    pub viewer_search: super::search::SearchState,
    selected_path: Option<std::path::PathBuf>,
    pending: PendingLogs,
    last_rescan: std::time::Instant,
    instances_dir_cache: Option<std::path::PathBuf>,
    was_live: bool,
}

impl Default for LogsState {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            list_state: TuiListState::default(),
            loaded_for: None,
            loading: false,
            viewer_focused: false,
            viewer_lines: Vec::new(),
            viewer_scroll: 0,
            viewer_max_scroll: 0,
            scrollbar_state: ScrollbarState::default(),
            viewer_scrollbar_state: ScrollbarState::default(),
            search: super::search::SearchState::default(),
            viewer_search: super::search::SearchState::default(),
            selected_path: None,
            pending: Arc::new(Mutex::new(None)),
            last_rescan: std::time::Instant::now(),
            instances_dir_cache: None,
            was_live: false,
        }
    }
}

impl LogsState {
    // drop loaded state tied to `name` so a rename forces a fresh scan.
    pub fn invalidate(&mut self, name: &str) {
        if self.loaded_for.as_deref() == Some(name) {
            self.loaded_for = None;
        }
    }

    /// Clears both the file-list and viewer search filters, returning true
    /// if either had a non-empty query. Resets the list selection to the top
    /// and exits the viewer pane if it was open, so the unfiltered file list
    /// is what's shown next.
    pub fn clear_search(&mut self) -> bool {
        let had = !self.search.is_empty() || !self.viewer_search.is_empty();
        self.search.deactivate();
        self.viewer_search.deactivate();
        if had {
            self.list_state.selected = Some(0);
            self.viewer_focused = false;
            self.update_scrollbar();
        }
        had
    }

    // true when the file list is showing and the selection is already at the
    // top. false while inside the log viewer, since k/Up there scrolls log
    // content instead. used to trigger instance rename from the content
    // header when the user keeps pressing k/Up past the top.
    pub fn is_at_top(&self) -> bool {
        !self.viewer_focused && self.list_state.selected.is_none_or(|s| s == 0)
    }

    pub fn start_load(&mut self, instances_dir: &Path, instance_name: &str) {
        self.loading = true;
        self.loaded_for = Some(instance_name.to_string());
        self.instances_dir_cache = Some(instances_dir.to_path_buf());
        self.entries.clear();
        self.list_state = TuiListState::default();
        self.viewer_lines.clear();
        self.viewer_scroll = 0;
        self.viewer_focused = false;
        self.selected_path = None;
        self.last_rescan = std::time::Instant::now();

        let dir = instances_dir.to_path_buf();
        let tag = instance_name.to_string();
        let pending = self.pending.clone();

        tokio::spawn(async move {
            let scan_dir = dir.clone();
            let scan_name = tag.clone();
            let entries =
                tokio::task::spawn_blocking(move || scan_log_files(&scan_dir, &scan_name))
                    .await
                    .unwrap_or_default();

            if let Ok(mut slot) = pending.lock() {
                *slot = Some((tag, entries));
                crate::tui::request_redraw();
            }
        });
    }

    pub fn drain_pending(&mut self) {
    // if the instance was live last frame and isn't now, and the live row
    // is still selected, the viewer's cached content is stale (it was left
    // blank while live, since the live row reads the ring buffer) — force
    // a reload of whatever the row now maps to.
        let live_now = self.has_live();
        if self.was_live && !live_now && self.list_state.selected == Some(0) {
            self.load_selected_content();
        }
        self.was_live = live_now;

        let taken = match self.pending.lock() {
            Ok(mut slot) => slot.take(),
            _ => None,
        };

        if let Some((instance_name, entries)) = taken
            && self.loaded_for.as_deref() == Some(&instance_name)
        {
            let prev_selected = self.list_state.selected;
            self.entries = entries;
            self.loading = false;

            let display_count = self.display_count();

            if display_count > 0 && prev_selected.is_none() {
                self.list_state.selected = Some(0);
                self.load_selected_content();
            } else if let Some(sel) = prev_selected
                && sel >= display_count
                && display_count > 0
            {
                self.list_state.selected = Some(display_count - 1);
            }
            self.update_scrollbar();
        }
    }

    // periodically re-scan log files in case new ones appeared while playing.
    pub fn try_rescan(&mut self) {
        if self.last_rescan.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        self.last_rescan = std::time::Instant::now();

        let (Some(dir), Some(name)) = (&self.instances_dir_cache, &self.loaded_for) else {
            return;
        };
        if !matches!(
            crate::running::get(name),
            Some(
                crate::running::RunState::Authenticating
                    | crate::running::RunState::Starting
                    | crate::running::RunState::Running
                    | crate::running::RunState::Orphaned(_)
            )
        ) {
            return;
        }

        let dir = dir.clone();
        let tag = name.clone();
        let pending = self.pending.clone();

        tokio::spawn(async move {
            let scan_dir = dir.clone();
            let scan_name = tag.clone();
            let entries =
                tokio::task::spawn_blocking(move || scan_log_files(&scan_dir, &scan_name))
                    .await
                    .unwrap_or_default();

            if let Ok(mut slot) = pending.lock() {
                *slot = Some((tag, entries));
                crate::tui::request_redraw();
            }
        });
    }

    // the real, on-disk directory Minecraft itself writes logs into
    // (`.minecraft/logs`) for the currently loaded instance -- used by the
    // ctrl+o "open dir" binding so it opens the actual log folder rather
    // than the generic instance root the other content tabs open.
    pub fn log_dir(&self) -> Option<std::path::PathBuf> {
        let dir = self.instances_dir_cache.as_ref()?;
        let name = self.loaded_for.as_deref()?;
        Some(crate::instance::log_files::log_dir(dir, name))
    }

    // a synthetic "Live" entry sits at index 0 while an instance is active
    // or just crashed, so parsed live-log styling is retained.
    //
    // Orphaned is deliberately excluded: those instances weren't spawned by
    // this session, so there's no stdout/stderr ring buffer to show — a
    // "Live" row would just render empty. latest.log on disk (kept fresh
    // by try_rescan) is the real content either way.
    fn has_live(&self) -> bool {
        let name = self.loaded_for.as_deref().unwrap_or("");
        matches!(
            crate::running::get(name),
            Some(crate::running::RunState::Running)
                | Some(crate::running::RunState::Starting)
                | Some(crate::running::RunState::Crashed(_))
        )
    }

    // indices into `self.entries` matching the file-list search query (or
    // all of them) — mirrors `ContentListState::filtered_indices`.
    //
    // while a "Live" row shows, `latest.log` is exactly what it's tailing,
    // so listing both would show the same session twice. hide the file
    // entry until the session ends and the Live row disappears; scanning
    // still keeps `self.entries` fresh underneath, so it reappears
    // immediately once has_live() goes false.
    pub fn filtered_indices(&self) -> Vec<usize> {
        let hide_latest = self.has_live();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.search.matches(&e.name))
            .filter(|(_, e)| !(hide_latest && e.name == "latest.log"))
            .map(|(i, _)| i)
            .collect()
    }

    pub(crate) fn display_count(&self) -> usize {
        self.filtered_indices().len() + if self.has_live() { 1 } else { 0 }
    }

    fn is_live_selected(&self) -> bool {
        self.has_live() && self.list_state.selected == Some(0)
    }

    // maps the current list selection to a real index into `self.entries`,
    // accounting for both the synthetic live row (offset) and the active
    // search filter.
    fn file_index_for_selected(&self) -> Option<usize> {
        let sel = self.list_state.selected?;
        let offset = if self.has_live() { 1 } else { 0 };
        if sel < offset {
            return None;
        }
        let filtered = self.filtered_indices();
        filtered.get(sel - offset).copied()
    }

    fn load_selected_content(&mut self) {
        if self.is_live_selected() {
            self.selected_path = None;
            self.viewer_lines.clear();
            self.viewer_scroll = 0;
            return;
        }

        let path = self
            .file_index_for_selected()
            .and_then(|i| self.entries.get(i))
            .map(|e| e.path.clone());

        if path == self.selected_path {
            return;
        }
        self.selected_path = path.clone();
        self.viewer_scroll = 0;

        if let Some(path) = path {
            self.viewer_lines = read_log_file(&path);
        } else {
            self.viewer_lines.clear();
        }
    }

    fn update_scrollbar(&mut self) {
        let count = self.display_count();
        let max = count.saturating_sub(1);
        let pos = self.list_state.selected.unwrap_or(0);
        self.scrollbar_state = ScrollbarState::new(max).position(pos);
    }

    fn update_viewer_scrollbar(&mut self, visible_height: usize, line_count: usize) {
        self.viewer_max_scroll = line_count.saturating_sub(visible_height);
        if self.viewer_scroll > self.viewer_max_scroll {
            self.viewer_scroll = self.viewer_max_scroll;
        }
        self.viewer_scrollbar_state =
            ScrollbarState::new(self.viewer_max_scroll).position(self.viewer_scroll);
    }

    pub fn pending_delete(
        &self,
    ) -> Option<crate::tui::widgets::content::list::PendingContentDelete> {
        let index = self.file_index_for_selected()?;
        // `self.entries[0]` (always `latest.log` after the descending sort)
        // is the file Minecraft's logger is actively writing. deleting it
        // out from under a live session is confusing at best, so block it
        // while live — regardless of which display row it maps to.
        if self.has_live() && index == 0 {
            return None;
        }
        let entry = self.entries.get(index)?;
        Some(crate::tui::widgets::content::list::PendingContentDelete {
            name: entry.name.clone(),
            path: entry.path.clone(),
        })
    }

    pub fn remove_path(&mut self, path: &Path) {
        self.entries.retain(|entry| entry.path != path);
        let display_count = self.display_count();
        if display_count == 0 {
            self.list_state.selected = None;
            self.viewer_focused = false;
            self.selected_path = None;
            self.viewer_lines.clear();
            self.viewer_scroll = 0;
        } else if let Some(sel) = self.list_state.selected {
            self.list_state.selected = Some(sel.min(display_count.saturating_sub(1)));
            self.load_selected_content();
        }
        self.update_scrollbar();
    }

    // builds the same text currently shown in the viewer pane (live ring
    // buffer or the loaded file), respecting an active `viewer_search`
    // filter -- so ctrl+c copies exactly what's on screen.
    fn visible_text(&self) -> String {
        let is_live = self.has_live() && self.list_state.selected == Some(0);

        let raw: Vec<String> = if is_live {
            let name = self.loaded_for.as_deref().unwrap_or("");
            crate::instance_logs::get_entries(name)
                .into_iter()
                .map(|line| line.text)
                .collect()
        } else {
            self.viewer_lines.clone()
        };

        raw.into_iter()
            .filter(|line| self.viewer_search.matches(line))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// OSC 52: ask the terminal emulator to put `text` on the system clipboard.
// works over SSH (a local clipboard crate has no X11/Wayland session to
// talk to on a remote box) wherever the emulator supports it (kitty,
// iTerm2, WezTerm, Windows Terminal, ...). silently no-ops elsewhere.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;

    if text.is_empty() {
        return;
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let sequence = format!("\x1b]52;c;{encoded}\x07");

    if let Err(e) = std::io::stdout().write_all(sequence.as_bytes()) {
        tracing::warn!("Failed to write clipboard OSC 52 sequence: {}", e);
        return;
    }
    let _ = std::io::stdout().flush();
}

pub fn handle_key(key_event: &KeyEvent, state: &mut LogsState) -> bool {
    use super::search::SearchAction;

    let shift = key_event.modifiers.contains(KeyModifiers::SHIFT);

    if state.viewer_focused {
        // checked before the search-input branch below so ctrl+c always
        // copies rather than leaking a literal 'c' into an active search
        // query.
        if key_event.code == KeyCode::Char('c')
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
        {
            copy_to_clipboard(&state.visible_text());
            return true;
        }

        let search_action = state.viewer_search.handle_key(key_event);
        match search_action {
            SearchAction::Unhandled => {}
            SearchAction::Activated
            | SearchAction::Edited
            | SearchAction::Confirmed
            | SearchAction::Deactivated => {
                state.viewer_scroll = 0;
                return true;
            }
            // every other key while searching is consumed by the search box
            SearchAction::Handled => return true,
        }

        match key_event.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if state.viewer_scroll < state.viewer_max_scroll {
                    state.viewer_scroll += 1;
                    state.viewer_scrollbar_state =
                        ScrollbarState::new(state.viewer_max_scroll).position(state.viewer_scroll);
                }
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                state.viewer_scroll = state.viewer_scroll.saturating_sub(1);
                state.viewer_scrollbar_state =
                    ScrollbarState::new(state.viewer_max_scroll).position(state.viewer_scroll);
                true
            }
            KeyCode::Char('G') => {
                state.viewer_scroll = state.viewer_max_scroll;
                state.viewer_scrollbar_state =
                    ScrollbarState::new(state.viewer_max_scroll).position(state.viewer_scroll);
                true
            }
            KeyCode::Char('g') => {
                state.viewer_scroll = 0;
                state.viewer_scrollbar_state =
                    ScrollbarState::new(state.viewer_max_scroll).position(state.viewer_scroll);
                true
            }
            // when the pane is empty there's nothing to "go back" to (the
            // file list is empty too), so let Esc fall through unconsumed to
            // the global kill-running-instance handler instead -- matches
            // the footer hint shown in that state.
            KeyCode::Esc if state.display_count() == 0 => false,
            KeyCode::Esc => {
                state.viewer_focused = false;
                true
            }
            KeyCode::Char('H') | KeyCode::Left if shift => {
                state.viewer_focused = false;
                true
            }
            // while reading a log, plain h/l/Left/Right mean nothing here --
            // swallow them so they don't bubble up to the global content
            // handler and yank the user to a different tab mid-read.
            KeyCode::Char('h') | KeyCode::Char('l') | KeyCode::Left | KeyCode::Right => true,
            _ => false,
        }
    } else {
        // checked before the search-input branch below for the same reason
        // as the viewer-focused case: don't let a literal 'c' leak into an
        // active search query.
        if key_event.code == KeyCode::Char('c')
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
        {
            copy_to_clipboard(&state.visible_text());
            return true;
        }

        let search_action = state.search.handle_key(key_event);
        match search_action {
            SearchAction::Unhandled => {}
            SearchAction::Activated
            | SearchAction::Edited
            | SearchAction::Confirmed
            | SearchAction::Deactivated => {
                state.list_state.selected = Some(0);
                state.update_scrollbar();
                return true;
            }
            // every other key while searching is consumed by the search box
            SearchAction::Handled => return true,
        }

        let display_count = state.display_count();
        match key_event.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if display_count == 0 {
                    return true;
                }
                let current = state.list_state.selected.unwrap_or(0);
                state.list_state.selected = Some((current + 1).min(display_count - 1));
                state.load_selected_content();
                state.update_scrollbar();
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let current = state.list_state.selected.unwrap_or(0);
                state.list_state.selected = Some(current.saturating_sub(1));
                state.load_selected_content();
                state.update_scrollbar();
                true
            }
            KeyCode::Enter => {
                state.viewer_focused = true;
                true
            }
            KeyCode::Char('L') | KeyCode::Right if shift => {
                state.viewer_focused = true;
                true
            }
            _ => false,
        }
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &mut LogsState, is_focused: bool) {
    let theme = THEME.as_ref();
    if state.loading {
        frame.render_widget(
            Paragraph::new("Loading logs...").style(Style::default().fg(theme.text_dim())),
            area,
        );
        return;
    }

    let has_live = state.has_live();
    let display_count = state.display_count();

    if display_count == 0 {
        frame.render_widget(
            Paragraph::new("No logs yet.").style(Style::default().fg(theme.text_dim())),
            area,
        );
        return;
    }

    if state.list_state.selected.is_none() && display_count > 0 {
        state.list_state.selected = Some(0);
        state.load_selected_content();
    }

    let [list_area, viewer_area] =
        Layout::horizontal([Constraint::Length(20), Constraint::Min(0)]).areas(area);

    render_list(frame, list_area, state, is_focused, has_live);
    render_viewer(frame, viewer_area, state, is_focused, has_live);
}

fn render_list(
    frame: &mut Frame,
    area: Rect,
    state: &mut LogsState,
    is_focused: bool,
    has_live: bool,
) {
    let theme = THEME.as_ref();
    let list_focused = is_focused && !state.viewer_focused;
    let border_color = if list_focused {
        theme.accent()
    } else {
        theme.border()
    };

    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_type(BORDER_STYLE.to_border_type())
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let display_count = state.display_count();
    let filtered = state.filtered_indices();

    // (name, is_live)
    let entries_snapshot: Vec<(String, bool)> = {
        let mut v = Vec::new();
        if has_live {
            // fixed label, not derived from the underlying file's name --
            // that file is always literally `latest.log` now (see
            // log_files.rs), so reusing its name here would just print
            // "latest" twice stacked on top of each other with nothing to
            // tell the two rows apart.
            v.push(("Live".to_string(), true));
        }
        for &i in &filtered {
            if let Some(e) = state.entries.get(i) {
                // strip a trailing .gz for display (e.g. "2026-08-02-1.log.gz"
                // -> "2026-08-02-1.log") -- the compressed/archived-ness
                // doesn't need its own badge, the name already reads fine
                // without it, and showing both was just ".gz" twice.
                let display = e.name.strip_suffix(".gz").unwrap_or(&e.name);
                v.push((display.to_string(), false));
            }
        }
        v
    };

    let builder = ListBuilder::new(move |context| {
        let (name, is_live) = &entries_snapshot[context.index];
        let show_selected = list_focused && context.is_selected;

        let style = if *is_live && show_selected {
            Style::default()
                .fg(theme.success())
                .add_modifier(Modifier::BOLD)
        } else if *is_live {
            Style::default().fg(theme.success())
        } else if show_selected {
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text())
        };

        let bg = if context.index % 2 == 0 {
            theme.background()
        } else {
            theme.stripe()
        };

        let selector = if show_selected {
            Span::styled("\u{258c} ", Style::default().fg(theme.accent()))
        } else {
            Span::raw("  ")
        };

        let item = ratatui::text::Text::from(Line::from(vec![
            selector,
            Span::styled(name.clone(), style),
        ]))
        .style(Style::default().bg(bg));
        (item, 1)
    });

    let list = ListView::new(builder, display_count);
    frame.render_stateful_widget(list, inner, &mut state.list_state);

    let scrollbar_area = Rect {
        x: inner.x + inner.width.saturating_sub(0),
        y: inner.y + 1,
        width: 1,
        height: inner.height.saturating_sub(2),
    };
    frame.render_stateful_widget(
        Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{25b2}"))
            .style(
                Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::BOLD),
            )
            .thumb_symbol("\u{2551}")
            .track_symbol(Some(""))
            .end_symbol(Some("\u{25bc}")),
        scrollbar_area,
        &mut state.scrollbar_state,
    );
}

fn render_viewer(
    frame: &mut Frame,
    area: Rect,
    state: &mut LogsState,
    _is_focused: bool,
    has_live: bool,
) {
    let theme = THEME.as_ref();
    let is_live = has_live && state.list_state.selected == Some(0);

    let all_lines: Vec<ViewerLine> = if is_live {
        let name = state.loaded_for.as_deref().unwrap_or("");
        crate::instance_logs::get_entries(name)
            .into_iter()
            .map(|line| ViewerLine {
                text: line.text,
                level: Some(line.level),
            })
            .collect()
    } else {
        state
            .viewer_lines
            .iter()
            .cloned()
            .map(|text| ViewerLine { text, level: None })
            .collect()
    };

    let lines: Vec<&ViewerLine> = all_lines
        .iter()
        .filter(|l| state.viewer_search.matches(&l.text))
        .collect();

    // one status row above the log content for the line-position stat; the
    // rest is the actual log text.
    let [status_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

    let visible_height = body_area.height as usize;
    // auto-scroll: if the user was already at the bottom, keep following
    // new lines as they come in (like `tail -f` behavior). this must be an
    // exact match, not "within a line or two" — a fuzzy threshold means a
    // single k/Up off the bottom (viewer_scroll == max-1) still counts as
    // "at bottom" and following immediately snaps the view back down,
    // making Up feel completely broken while a session is live. g/G worked
    // around it before this fix only because they jump scroll to 0 (or to
    // whatever max_scroll was *last frame*), which is far enough from the
    // old threshold to actually stick.
    let was_at_bottom = state.viewer_scroll >= state.viewer_max_scroll;
    state.update_viewer_scrollbar(visible_height, lines.len());

    let following = is_live && was_at_bottom && !state.viewer_search.active;
    if following {
        state.viewer_scroll = state.viewer_max_scroll;
        state.viewer_scrollbar_state =
            ScrollbarState::new(state.viewer_max_scroll).position(state.viewer_scroll);
    }

    render_viewer_status(frame, status_area, state, is_live, &lines, visible_height);

    if lines.is_empty() {
        return;
    }

    let search = &state.viewer_search;
    let styled_lines: Vec<Line> = lines
        .iter()
        .skip(state.viewer_scroll)
        .take(visible_height)
        .map(|line| {
            search.highlight_line(
                &line.text,
                line.level
                    .map(log_level_style)
                    .unwrap_or_else(|| line_level_style(&line.text)),
            )
        })
        .collect();

    // wrap long lines instead of truncating at the terminal edge — wide
    // log lines (long classpaths, stack traces, mixin refmap paths) were
    // getting cut off and only readable by shrinking the terminal font.
    frame.render_widget(
        Paragraph::new(styled_lines).wrap(Wrap { trim: false }),
        body_area,
    );

    let scrollbar_area = Rect {
        x: body_area.x + body_area.width.saturating_sub(0),
        y: body_area.y + 1,
        width: 1,
        height: body_area.height.saturating_sub(2),
    };
    frame.render_stateful_widget(
        Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{25b2}"))
            .style(
                Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::BOLD),
            )
            .thumb_symbol("\u{2551}")
            .track_symbol(Some(""))
            .end_symbol(Some("\u{25bc}")),
        scrollbar_area,
        &mut state.viewer_scrollbar_state,
    );
}

// right-aligned line-position stat ("120-160 / 482") and, for a
// compressed archive, its decompressed size.
fn render_viewer_status(
    frame: &mut Frame,
    area: Rect,
    state: &LogsState,
    is_live: bool,
    lines: &[&ViewerLine],
    visible_height: usize,
) {
    let theme = THEME.as_ref();

    let total = lines.len();
    let (start, end) = if total == 0 {
        (0, 0)
    } else {
        let start = state.viewer_scroll + 1;
        let end = (state.viewer_scroll + visible_height.max(1)).min(total);
        (start, end)
    };

    let size_suffix = if !is_live {
        state
            .file_index_for_selected()
            .and_then(|i| state.entries.get(i))
            .filter(|e| e.compressed)
            .map(|_| {
                let bytes: usize = lines.iter().map(|l| l.text.len() + 1).sum();
                format!(" \u{00b7} {}", format_bytes(bytes))
            })
            .unwrap_or_default()
    } else {
        String::new()
    };

    let stat_text = format!("{start}-{end} / {total}{size_suffix}");
    frame.render_widget(
        Paragraph::new(Span::styled(stat_text, Style::default().fg(theme.text_dim())))
            .alignment(Alignment::Right),
        area,
    );
}

fn format_bytes(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

struct ViewerLine {
    text: String,
    level: Option<LogLevel>,
}

fn log_level_style(level: LogLevel) -> Style {
    let theme = THEME.as_ref();
    match level {
        LogLevel::Error => Style::default().fg(theme.error()),
        LogLevel::Warn => Style::default().fg(theme.warning()),
        LogLevel::Debug | LogLevel::Trace => Style::default().fg(theme.text_dim()),
        LogLevel::Info => Style::default().fg(theme.text()),
    }
}

// color-code log lines by severity so errors actually stand out
// instead of drowning in a wall of white text
fn line_level_style(line: &str) -> Style {
    let theme = THEME.as_ref();
    let upper = line.to_uppercase();
    if upper.contains("ERROR") || upper.contains("FATAL") {
        Style::default().fg(theme.error())
    } else if upper.contains("WARN") {
        Style::default().fg(theme.warning())
    } else if upper.contains("DEBUG") || upper.contains("TRACE") {
        Style::default().fg(theme.text_dim())
    } else {
        Style::default().fg(theme.text())
    }
}
