// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// the tabbed content area: mods, resource packs, shaders, screenshots, worlds, logs

pub mod list;
pub mod settings;
pub mod tabs;

pub use list::{ContentListState, handle_key, handle_key_no_toggle};
pub use tabs::{ContentTab, render, title};

/// the whole content area as one unit: the active tab plus every tab's
/// state. App holds a single `content: ContentArea` instead of five loose
/// state fields, and the per-frame plumbing (drain every pipeline's
/// stream/watcher/image queues) collapses into one `tick()` call.
pub struct ContentArea {
    pub tab: ContentTab,
    pub mods: list::ContentListState,
    pub resource_packs: list::ContentListState,
    pub worlds: list::ContentListState,
    pub screenshots: crate::tui::widgets::screenshots_grid::ScreenshotsState,
    pub logs: crate::tui::widgets::logs_viewer::LogsState,
    pub settings: settings::SettingsTabState,
}

impl Default for ContentArea {
    fn default() -> Self {
        Self {
            tab: ContentTab::default(),
            mods: list::ContentListState::default(),
            resource_packs: list::ContentListState::default(),
            worlds: list::ContentListState::default(),
            screenshots: crate::tui::widgets::screenshots_grid::ScreenshotsState::default(),
            logs: crate::tui::widgets::logs_viewer::LogsState::default(),
            settings: settings::SettingsTabState::default(),
        }
    }
}

impl ContentArea {
    /// per-frame plumbing: drain every content pipeline's streaming/
    /// watcher/image queues and turn freshly-decoded thumbnails into
    /// terminal protocols (must happen on the main thread). one call
    /// replaces the ~16 drain calls that used to litter the event loop.
    pub fn tick(&mut self, picker: &ratatui_image::picker::Picker) {
        self.mods.drain_pending();
        self.mods.drain_watcher();
        self.mods.request_image_loads(picker);
        self.mods.drain_image_loads(picker);

        self.resource_packs.drain_pending();
        self.resource_packs.drain_watcher();
        self.resource_packs.request_image_loads(picker);
        self.resource_packs.drain_image_loads(picker);

        self.worlds.drain_pending();
        self.worlds.drain_watcher();
        self.worlds.request_image_loads(picker);
        self.worlds.drain_image_loads(picker);

        self.screenshots.drain_pending_entries();
        self.screenshots.request_visible_loads();
        let pending = self.screenshots.take_pending_images();
        for (idx, img) in pending {
            match img {
                Some(img) => {
                    let proto = picker.new_resize_protocol(img);
                    self.screenshots.set_protocol(idx, proto);
                }
                None => self.screenshots.mark_failed(idx),
            }
        }

        self.logs.drain_pending();
        self.logs.try_rescan();
    }

    /// clears any per-tab cached/loaded state tied to `name`, so every tab
    /// does a fresh scan after an instance rename instead of reusing stale
    /// state left over under the old name.
    pub fn invalidate(&mut self, name: &str) {
        self.mods.invalidate(name);
        self.resource_packs.invalidate(name);
        self.worlds.invalidate(name);
        self.screenshots.invalidate(name);
        self.logs.invalidate(name);
    }

    pub fn invalidate_image_protocols(&mut self) {
        self.mods.invalidate_image_protocols();
        self.resource_packs.invalidate_image_protocols();
        self.worlds.invalidate_image_protocols();
        self.screenshots.invalidate_protocols();
    }

    /// Clears the search filter on the currently active tab, returning true
    /// if a non-empty filter was actually cleared. Selection resets to the
    /// top of the now-unfiltered list. Called from:
    /// - Esc in a content tab (priority over the global kill binding)
    /// - switching tabs (so a filter doesn't persist across tabs)
    pub fn clear_search(&mut self) -> bool {
        match self.tab {
            ContentTab::Mods => self.mods.clear_search(),
            ContentTab::ResourcePacks => self.resource_packs.clear_search(),
            ContentTab::Worlds => self.worlds.clear_search(),
            ContentTab::Screenshots => {
                let had = !self.screenshots.search.is_empty();
                self.screenshots.search.deactivate();
                if had {
                    self.screenshots.selected = 0;
                }
                had
            }
            ContentTab::Logs => self.logs.clear_search(),
            ContentTab::Settings => false,
        }
    }

    /// Returns true if the active tab has a non-empty search filter
    /// (whether the search box is open or the filter is just retained after
    /// Enter). Used by the tab footer to swap the Esc binding label.
    pub fn search_active(&self) -> bool {
        match self.tab {
            ContentTab::Mods => !self.mods.search.is_empty(),
            ContentTab::ResourcePacks => !self.resource_packs.search.is_empty(),
            ContentTab::Worlds => !self.worlds.search.is_empty(),
            ContentTab::Screenshots => !self.screenshots.search.is_empty(),
            ContentTab::Logs => {
                !self.logs.search.is_empty() || !self.logs.viewer_search.is_empty()
            }
            ContentTab::Settings => false,
        }
    }

    /// true when the active tab's selection is already at the top (or the
    /// list is empty), meaning another k/Up should open the instance rename
    /// field in the content header instead of moving the selection.
    pub fn at_top(&self) -> bool {
        match self.tab {
            ContentTab::Mods => !self.mods.search.active && self.mods.is_at_top(),
            ContentTab::ResourcePacks => {
                !self.resource_packs.search.active && self.resource_packs.is_at_top()
            }
            ContentTab::Worlds => !self.worlds.search.active && self.worlds.is_at_top(),
            ContentTab::Screenshots => {
                !self.screenshots.search.active && self.screenshots.is_at_top()
            }
            ContentTab::Logs => {
                !self.logs.search.active
                    && !self.logs.viewer_search.active
                    && self.logs.is_at_top()
            }
            ContentTab::Settings => self.settings.is_at_top(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_search_returns_false_when_no_filter() {
        let content = ContentArea::default();
        assert!(!content.clear_search());
    }

    #[test]
    fn clear_search_clears_mods_confirmed_filter() {
        let mut content = ContentArea::default();
        content.tab = ContentTab::Mods;
        content.mods.search.activate();
        content.mods.search.query = "fabric".to_string();
        content.mods.search.confirm();
        assert!(!content.mods.search.is_empty());

        assert!(content.clear_search());
        assert!(content.mods.search.is_empty());
        assert!(!content.mods.search.active);
    }

    #[test]
    fn clear_search_clears_screenshots_filter() {
        let mut content = ContentArea::default();
        content.tab = ContentTab::Screenshots;
        content.screenshots.search.activate();
        content.screenshots.search.query = "cave".to_string();
        content.screenshots.search.confirm();
        assert!(!content.screenshots.search.is_empty());

        assert!(content.clear_search());
        assert!(content.screenshots.search.is_empty());
        assert!(!content.screenshots.search.active);
        assert_eq!(content.screenshots.selected, 0);
    }

    #[test]
    fn search_active_detects_mods_filter() {
        let mut content = ContentArea::default();
        content.tab = ContentTab::Mods;
        assert!(!content.search_active());
        content.mods.search.activate();
        content.mods.search.query = "x".to_string();
        content.mods.search.confirm();
        assert!(content.search_active());
        content.clear_search();
        assert!(!content.search_active());
    }

    #[test]
    fn search_active_false_for_settings() {
        let content = ContentArea::default();
        assert!(!content.search_active());
    }
}
