// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// keybindings and input dispatch.
// the general pattern: check which area is focused, give it first crack at the
// keypress, and fall through to global bindings if nobody claimed it.
// vim-style navigation (j/k/g/G) where it makes sense.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, FocusedArea, JavaSelectTarget};
use super::widgets::{
    self, popups::confirm as confirm_popup, popups::content_browse, popups::description,
    popups::global_settings, popups::java_select, popups::new_instance,
};
use crate::tui::error_buffer;

impl App {
    pub(super) fn handle_key_event(&mut self, key_event: KeyEvent) -> color_eyre::Result<()> {
        // log overlay eats all input when open. search keys go through the
        // shared SearchState loop ('/' activates, every key while searching
        // is consumed), then the overlay's scroll/close keys handle the rest.
        if self.focused == FocusedArea::OverviewExpanded {
            let search_action = self.log_overlay_search.handle_key(&key_event);
            if !matches!(
                search_action,
                crate::tui::widgets::search::SearchAction::Unhandled
            ) {
                return Ok(());
            }
            match key_event.code {
                KeyCode::Char('O') | KeyCode::Esc => {
                    self.focused = self.pre_overlay_focused;
                    self.log_overlay_search.deactivate();
                    return Ok(());
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if self.log_overlay_scroll < self.log_overlay_max_scroll {
                        self.log_overlay_scroll += 1;
                    }
                    return Ok(());
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.log_overlay_scroll = self.log_overlay_scroll.saturating_sub(1);
                    return Ok(());
                }
                KeyCode::Char('G') => {
                    self.log_overlay_scroll = self.log_overlay_max_scroll;
                    return Ok(());
                }
                KeyCode::Char('g') => {
                    self.log_overlay_scroll = 0;
                    return Ok(());
                }
                _ => {
                    return Ok(());
                }
            }
        }

        // the project description view is modal over the browse popups —
        // when it's open it eats keypresses (scroll/close), except i/v which
        // close it and pass through to the popup's own handlers.
        if description::is_open() {
            match description::handle_key(&key_event) {
                description::KeyAction::Consumed => return Ok(()),
                description::KeyAction::Passthrough => {} // fall through
            }
        }

        if self.focused == FocusedArea::ConfirmDelete {
            match key_event.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let focus_after = match confirm_popup::pending_target() {
                        Some(confirm_popup::ConfirmTarget::Instance { name }) => {
                            match self.instance_manager.delete(&name) {
                                Ok(_) => {
                                    self.instances_state.remove_instance(&name);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to delete instance '{}': {}", name, e);
                                }
                            }
                            FocusedArea::Instances
                        }
                        Some(confirm_popup::ConfirmTarget::Account { index, .. }) => {
                            let count = self.account_state.store.accounts.len();
                            self.account_state.store.remove(index);
                            if count > 1 {
                                self.account_state.list_state.selected = Some(index.min(
                                    self.account_state.store.accounts.len().saturating_sub(1),
                                ));
                            } else {
                                self.account_state.list_state.selected = None;
                            }
                            FocusedArea::Account
                        }
                        Some(confirm_popup::ConfirmTarget::Content { name, path }) => {
                            match delete_content_path(&path) {
                                Ok(()) => {
                                    self.remove_content_path_from_states(&path);
                                    self.forget_installed_meta(&path);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to delete content '{}': {}", name, e);
                                }
                            }
                            FocusedArea::Content
                        }
                        None => FocusedArea::Instances,
                    };
                    confirm_popup::clear_pending();
                    self.focused = focus_after;
                    return Ok(());
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    let focus_after = match confirm_popup::pending_target() {
                        Some(confirm_popup::ConfirmTarget::Content { .. }) => FocusedArea::Content,
                        Some(confirm_popup::ConfirmTarget::Account { .. }) => FocusedArea::Account,
                        _ => FocusedArea::Instances,
                    };
                    confirm_popup::clear_pending();
                    self.focused = focus_after;
                    return Ok(());
                }
                _ => {
                    return Ok(());
                }
            }
        }

        if self.focused == FocusedArea::JavaSelect {
            match java_select::handle_key(&key_event) {
                java_select::JavaSelectAction::Pick(path) => {
                    match self.java_select_target {
                        JavaSelectTarget::Instance => {
                            if let Some(instance) = self.instances_state.selected_instance() {
                                let mut updated = instance.clone();
                                updated.java_path = Some(path);
                                let name = updated.name.clone();
                                if let Err(e) = self.instance_manager.save(&updated) {
                                    error_buffer::push_error(error_buffer::ErrorEvent {
                                        id: 0,
                                        level: tracing::Level::ERROR,
                                        message: format!("Failed to save settings: {e}"),
                                        pushed_at: std::time::Instant::now(),
                                    });
                                } else {
                                    self.instances_state.replace_instance(&name, updated);
                                }
                            }
                            java_select::close();
                            self.focused = FocusedArea::Content;
                        }
                        JavaSelectTarget::Global => {
                            global_settings::set_java_path(Some(path));
                            java_select::close();
                            self.focused = FocusedArea::GlobalSettings;
                        }
                    }
                    return Ok(());
                }
                java_select::JavaSelectAction::Cancel => {
                    java_select::close();
                    self.focused = match self.java_select_target {
                        JavaSelectTarget::Instance => FocusedArea::Content,
                        JavaSelectTarget::Global => FocusedArea::GlobalSettings,
                    };
                    return Ok(());
                }
                java_select::JavaSelectAction::None => {
                    return Ok(());
                }
            }
        }

        if self.focused == FocusedArea::GlobalSettings {
            match global_settings::handle_key(&key_event) {
                global_settings::GlobalSettingsAction::None => {}
                global_settings::GlobalSettingsAction::Error(message) => {
                    error_buffer::push_error(error_buffer::ErrorEvent {
                        id: 0,
                        level: tracing::Level::ERROR,
                        message,
                        pushed_at: std::time::Instant::now(),
                    });
                }
                global_settings::GlobalSettingsAction::OpenJavaPicker(current) => {
                    self.java_select_target = JavaSelectTarget::Global;
                    java_select::open(current);
                    self.focused = FocusedArea::JavaSelect;
                }
                global_settings::GlobalSettingsAction::Close => {
                    global_settings::close();
                    self.focused = FocusedArea::Instances;
                }
                global_settings::GlobalSettingsAction::CloseAndRestart => {
                    global_settings::close();
                    self.restart_requested = true;
                    self.exit = true;
                }
            }
            return Ok(());
        }

        // Ctrl+Enter launches from either the sidebar or content view, so
        // you don't have to hop back to the sidebar to hit play. checked
        // before both areas' own dispatch so plain Enter (toggle/focus)
        // can't swallow it.
        if key_event.code == KeyCode::Enter
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(self.focused, FocusedArea::Instances | FocusedArea::Content)
            && !self.instances_state.search.active
            && self.content_renaming.is_none()
        {
            if let Some(instance) = self.instances_state.selected_instance().cloned() {
                // only allow launching if instance isn't already running.
                // crashed instances can be relaunched (clears old state first)
                let can_launch = matches!(
                    crate::running::get(&instance.name),
                    None | Some(crate::running::RunState::Crashed(_))
                );
                if can_launch {
                    crate::running::remove(&instance.name);
                    crate::instance_logs::clear(&instance.name);
                    self.spawn_launch(instance);
                }
            }
            return Ok(());
        }

        // content area delegates to whichever tab is active.
        // worlds use the same list navigation without the toggle
        if self.focused == FocusedArea::Content {
            // editing the instance name in the content header, opened below
            // by k/Up at the top of the list. the edit loop is shared with
            // the sidebar's rename field via handle_rename_key.
            if self.content_renaming.is_some() {
                match handle_rename_key(&key_event, &mut self.content_renaming) {
                    RenameKey::Commit => {
                        let new_name = self.content_renaming.take().unwrap_or_default();
                        if let Some(inst) = self.instances_state.selected_instance() {
                            let old_name = inst.name.clone();
                            if let Ok(()) = self.instance_manager.rename(&old_name, &new_name)
                                && let Some(inst) = self
                                    .instances_state
                                    .instances
                                    .iter_mut()
                                    .find(|i| i.name == old_name)
                            {
                                inst.name = new_name.trim().to_owned();
                                self.content.invalidate(&old_name);
                            }
                        }
                    }
                    RenameKey::Cancel => {
                        self.content_renaming = None;
                    }
                    RenameKey::Edit | RenameKey::Other => {}
                }
                return Ok(());
            }

            // k/Up at the top of the list opens the rename field, so the
            // navigation key also reaches "past" the list to the header.
            if matches!(key_event.code, KeyCode::Char('k') | KeyCode::Up) && self.content_at_top()
            {
                if let Some(inst) = self.instances_state.selected_instance() {
                    self.content_renaming = Some(inst.name.clone());
                }
                return Ok(());
            }

            // 'd' opens the delete confirmation on every tab with deletable
            // content, guarded by "not currently searching" (settings has
            // nothing to delete). used to be a per-tab copy of this guard.
            if key_event.code == KeyCode::Char('d')
                && self.content_delete_search_inactive()
                && let Some(pending) = self.content_pending_delete()
            {
                confirm_popup::set_pending_content_delete(pending.name, pending.path);
                self.focused = FocusedArea::ConfirmDelete;
                return Ok(());
            }

            match self.content.tab {
                widgets::content::ContentTab::Logs => {
                    if widgets::logs_viewer::handle_key(&key_event, &mut self.content.logs) {
                        return Ok(());
                    }
                }
                widgets::content::ContentTab::Screenshots => {
                    if widgets::screenshots_grid::handle_key(&key_event, &mut self.content.screenshots)
                    {
                        return Ok(());
                    }
                }
                widgets::content::ContentTab::Worlds => {
                    if widgets::content::list::handle_key_no_toggle(
                        &key_event,
                        &mut self.content.worlds,
                    ) {
                        return Ok(());
                    }
                }
                widgets::content::ContentTab::Settings => {
                    match widgets::content::settings::handle_key(
                        &key_event,
                        &mut self.content.settings,
                        self.instances_state.selected_instance(),
                    ) {
                    widgets::content::settings::SettingsTabAction::UpdateInstance(updated) => {
                        let name = updated.name.clone();
                        if let Err(e) = self.instance_manager.save(&updated) {
                            error_buffer::push_error(error_buffer::ErrorEvent {
                                id: 0,
                                level: tracing::Level::ERROR,
                                message: format!("Failed to save settings: {e}"),
                                pushed_at: std::time::Instant::now(),
                            });
                        } else {
                            self.instances_state.replace_instance(&name, updated);
                        }
                        return Ok(());
                    }
                    widgets::content::settings::SettingsTabAction::Error(message) => {
                        error_buffer::push_error(error_buffer::ErrorEvent {
                            id: 0,
                            level: tracing::Level::ERROR,
                            message,
                            pushed_at: std::time::Instant::now(),
                        });
                        return Ok(());
                    }
                    widgets::content::settings::SettingsTabAction::None => {
                        return Ok(());
                    }
                    // don't return: let it fall through to the global
                    // dispatcher below (h/l tab switching, I/C/A focus
                    // changes, q to quit, etc.)
                    widgets::content::settings::SettingsTabAction::Unhandled => {}
                    widgets::content::settings::SettingsTabAction::OpenJavaPicker => {
                        let current = self
                            .instances_state
                            .selected_instance()
                            .and_then(|i| i.java_path.clone());
                        self.java_select_target = JavaSelectTarget::Instance;
                        java_select::open(current);
                        self.focused = FocusedArea::JavaSelect;
                        return Ok(());
                    }
                    }
                }
                widgets::content::ContentTab::Mods | widgets::content::ContentTab::ResourcePacks => {
                    if let Some(state) = self.active_content_list_state()
                        && widgets::content::list::handle_key(&key_event, state)
                    {
                        return Ok(());
                    }
                }
            }
        }

        // only when no add-account popup is open — while the offline-name
        // entry or the type chooser is up, 'd' belongs to the popup (e.g. a
        // 'd' in the username), not to the delete hotkey.
        if self.focused == FocusedArea::Account
            && !self.account_state.popup_open()
            && let KeyCode::Char('d') = key_event.code
            && let Some(index) = self.account_state.list_state.selected
            && let Some(account) = self.account_state.store.accounts.get(index)
        {
            confirm_popup::set_pending(confirm_popup::ConfirmTarget::Account {
                username: account.username.clone(),
                index,
            });
            self.focused = FocusedArea::ConfirmDelete;
            return Ok(());
        }

        if self.focused == FocusedArea::Account
            && widgets::account::handle_key(&key_event, &mut self.account_state)
        {
            return Ok(());
        }

        match self.focused {
            FocusedArea::Popup => {
                new_instance::handle_key(&key_event, &mut self.instances_state);
            }
            FocusedArea::ContentBrowse => {
                if content_browse::handle_key(&key_event) {
                    self.focused = FocusedArea::Content;
                }
                return Ok(());
            }
            _ => {
                if self.focused == FocusedArea::Instances && self.instances_state.renaming.is_some()
                {
                    match handle_rename_key(&key_event, &mut self.instances_state.renaming) {
                        RenameKey::Commit => {
                            let new_name = self.instances_state.renaming.take().unwrap_or_default();
                            if let Some(inst) = self.instances_state.selected_instance() {
                                let old_name = inst.name.clone();
                                if let Ok(()) = self.instance_manager.rename(&old_name, &new_name)
                                    && let Some(inst) = self
                                        .instances_state
                                        .instances
                                        .iter_mut()
                                        .find(|i| i.name == old_name)
                                {
                                    inst.name = new_name.trim().to_owned();
                                    self.content.invalidate(&old_name);
                                }
                            }
                        }
                        RenameKey::Cancel => {
                            self.instances_state.renaming = None;
                        }
                        RenameKey::Edit | RenameKey::Other => {}
                    }
                    return Ok(());
                }

                if self.focused == FocusedArea::Instances && self.instances_state.search.active {
                    // search mode consumes every key; the return value is
                    // irrelevant since we swallow input either way
                    let _ = widgets::instances::handle_key(&key_event, &mut self.instances_state);
                    return Ok(());
                }

                // global keybindings (uppercase = area switch, lowercase = action)
                match key_event.code {
                    KeyCode::Char('q') => self.exit = true,
                    KeyCode::Char('I') => self.focused = FocusedArea::Instances,
                    KeyCode::Char('C') => self.focused = FocusedArea::Content,
                    KeyCode::Char('A') => self.focused = FocusedArea::Account,
                    KeyCode::Char('O') => {
                        self.pre_overlay_focused = self.focused;
                        self.focused = FocusedArea::OverviewExpanded;
                    }
                    KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right
                        if self.focused == FocusedArea::Content =>
                    {
                        self.content.tab = self.content.tab.next();
                    }
                    KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left
                        if self.focused == FocusedArea::Content =>
                    {
                        self.content.tab = self.content.tab.previous();
                    }
                    KeyCode::Char('b')
                        if self.focused == FocusedArea::Content
                            && matches!(
                                self.content.tab,
                                widgets::content::ContentTab::Mods
                                    | widgets::content::ContentTab::ResourcePacks
                            ) =>
                    {
                        self.open_content_browse();
                    }
                    KeyCode::Char('d')
                        if self.focused == FocusedArea::Instances
                            && !self.instances_state.search.active =>
                    {
                        if let Some(instance) = self.instances_state.selected_instance() {
                            let name = instance.name.clone();
                            confirm_popup::set_pending_instance_delete(&name);
                            self.focused = FocusedArea::ConfirmDelete;
                        }
                    }
                    // global (launcher-wide) settings -- config.toml, not to
                    // be confused with the per-instance Settings content tab
                    KeyCode::Char('s')
                        if self.focused == FocusedArea::Instances
                            && !self.instances_state.search.active =>
                    {
                        global_settings::open();
                        self.focused = FocusedArea::GlobalSettings;
                    }
                    // ctrl+o = open .minecraft folder in the file manager.
                    // (was shift+enter, but most terminals can't report that
                    // distinctly without kitty keyboard protocol support, so
                    // ctrl+o — it works everywhere). also fires from the
                    // Content area, where the footer advertised it but
                    // nothing responded to it.
                    KeyCode::Char('o')
                        if key_event.modifiers.contains(KeyModifiers::CONTROL)
                            && ((self.focused == FocusedArea::Instances
                                && !self.instances_state.search.active)
                                || self.focused == FocusedArea::Content) =>
                    {
                        if self.focused == FocusedArea::Content {
                            self.open_content_tab_dir();
                        } else {
                            self.open_selected_instance_dir();
                        }
                    }
                    // plain enter (or right arrow) = focus the content area
                    // for the selected instance
                    KeyCode::Enter | KeyCode::Right
                        if self.focused == FocusedArea::Instances
                            && !self.instances_state.search.active =>
                    {
                        self.focused = FocusedArea::Content;
                    }
                    KeyCode::Char('r')
                        if self.focused == FocusedArea::Instances
                            && !self.instances_state.search.active =>
                    {
                        if let Some(inst) = self.instances_state.selected_instance() {
                            self.instances_state.renaming = Some(inst.name.clone());
                        }
                    }
                    // esc = kill running instance. brutal but effective
                    KeyCode::Esc
                        if matches!(
                            self.focused,
                            FocusedArea::Instances | FocusedArea::Content
                        ) && !self.instances_state.search.active =>
                    {
                        if let Some(instance) = self.instances_state.selected_instance() {
                            crate::running::send_kill(&instance.name);
                        }
                    }
                    _ => {}
                }

                if self.focused == FocusedArea::Instances {
                    // last handler in the chain; nothing follows to consume
                    // the result
                    let _ = widgets::instances::handle_key(&key_event, &mut self.instances_state);
                }
            }
        }

        if self.instances_state.wants_popup() {
            self.focused = FocusedArea::Popup;
        } else if self.focused == FocusedArea::Popup {
            self.focused = FocusedArea::Instances;
        }

        Ok(())
    }

    // a content file the user deleted (mods tab 'd', resource packs, etc.)
    // may be the file the installed-content sidecar associates with a
    // catalog project — drop that record so the browse popup stops showing
    // an "installed" badge for something that no longer exists. dirs
    // (worlds) and log files are never tracked, so they're skipped by the
    // mods/resourcepacks parent check inside.
    fn forget_installed_meta(&self, deleted_path: &std::path::Path) {
        let Some((content_dir, stem)) = crate::instance::content::installed_meta::content_dir_and_stem(deleted_path)
        else {
            return;
        };
        crate::instance::content::installed_meta::remove_stems(&content_dir, &[stem]);
        content_browse::refresh_installed(&content_dir);
    }

    fn remove_content_path_from_states(&mut self, path: &std::path::Path) {
        self.content.mods.remove_path(path);
        self.content.resource_packs.remove_path(path);
        self.content.worlds.remove_path(path);
        self.content.screenshots.remove_path(path);
        self.content.logs.remove_path(path);
    }

    // per-tab "not currently searching" guard for the 'd' delete key.
    fn content_delete_search_inactive(&self) -> bool {
        match self.content.tab {
            widgets::content::ContentTab::Logs => {
                !self.content.logs.search.active && !self.content.logs.viewer_search.active
            }
            widgets::content::ContentTab::Screenshots => !self.content.screenshots.search.active,
            widgets::content::ContentTab::Worlds => !self.content.worlds.search.active,
            widgets::content::ContentTab::Mods => !self.content.mods.search.active,
            widgets::content::ContentTab::ResourcePacks => {
                !self.content.resource_packs.search.active
            }
            widgets::content::ContentTab::Settings => false,
        }
    }

    // the selected content entry on the active tab, if any - what 'd'
    // would delete.
    fn content_pending_delete(
        &self,
    ) -> Option<crate::tui::widgets::content::list::PendingContentDelete> {
        match self.content.tab {
            widgets::content::ContentTab::Logs => self.content.logs.pending_delete(),
            widgets::content::ContentTab::Screenshots => self.content.screenshots.pending_delete(),
            widgets::content::ContentTab::Worlds => self.content.worlds.pending_delete(),
            widgets::content::ContentTab::Mods => self.content.mods.pending_delete(),
            widgets::content::ContentTab::ResourcePacks => {
                self.content.resource_packs.pending_delete()
            }
            widgets::content::ContentTab::Settings => None,
        }
    }

    // the shared list state behind the Mods / Resource Packs tabs.
    fn active_content_list_state(
        &mut self,
    ) -> Option<&mut widgets::content::list::ContentListState> {
        match self.content.tab {
            widgets::content::ContentTab::Mods => Some(&mut self.content.mods),
            widgets::content::ContentTab::ResourcePacks => Some(&mut self.content.resource_packs),
            _ => None,
        }
    }

    fn open_selected_instance_dir(&self) {
        if let Some(instance) = self.instances_state.selected_instance() {
            let dir = self
                .instance_manager
                .instances_dir
                .join(&instance.name)
                .join(".minecraft");
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::error!("Failed to create instance directory {}: {}", dir.display(), e);
                return;
            }
            if let Err(e) = open::that_detached(&dir) {
                tracing::error!("Failed to open instance directory: {}", e);
            }
        }
    }

    // 'b' from the Mods/Resource Packs tabs: opens the browse-and-install
    // popup scoped to the active instance/tab, so search and version
    // filtering can be narrowed to what's actually compatible.
    fn open_content_browse(&mut self) {
        let Some(instance) = self.instances_state.selected_instance() else {
            return;
        };

        let kind = match self.content.tab {
            widgets::content::ContentTab::Mods => content_browse::ContentKind::Mod,
            widgets::content::ContentTab::ResourcePacks => content_browse::ContentKind::ResourcePack,
            _ => return,
        };
        let subdir = match kind {
            content_browse::ContentKind::Mod => "mods",
            content_browse::ContentKind::ResourcePack => "resourcepacks",
        };
        let dest_dir = self
            .instance_manager
            .instances_dir
            .join(&instance.name)
            .join(".minecraft")
            .join(subdir);

        content_browse::open(
            kind,
            instance.name.clone(),
            dest_dir,
            instance.game_version.clone(),
            instance.loader,
        );
        self.focused = FocusedArea::ContentBrowse;
    }

    // ctrl+o from the Content area: the Logs tab opens the real
    // `.minecraft/logs` directory (where latest.log and the rotated .log.gz
    // archives live) instead of the generic instance root.
    fn open_content_tab_dir(&self) {
        if self.content.tab == widgets::content::ContentTab::Logs
            && let Some(dir) = self.content.logs.log_dir()
        {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::error!("Failed to create logs directory {}: {}", dir.display(), e);
                return;
            }
            if let Err(e) = open::that_detached(&dir) {
                tracing::error!("Failed to open logs directory: {}", e);
            }
            return;
        }
        self.open_selected_instance_dir();
    }
}

fn delete_content_path(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// what a keypress did to an inline rename field (sidebar or content
// header). both callers used to duplicate this ~25-line loop; it lives
// here once now.
enum RenameKey {
    Commit,
    Cancel,
    Edit,
    Other,
}

fn handle_rename_key(key_event: &KeyEvent, field: &mut Option<String>) -> RenameKey {
    match key_event.code {
        KeyCode::Enter => RenameKey::Commit,
        // Down cancels in the sidebar too now, matching the content header's
        // existing behavior, instead of being a silent no-op.
        KeyCode::Esc | KeyCode::Down => RenameKey::Cancel,
        KeyCode::Backspace => {
            if let Some(name) = field {
                name.pop();
            }
            RenameKey::Edit
        }
        KeyCode::Char(c) => {
            if let Some(name) = field {
                name.push(c);
            }
            RenameKey::Edit
        }
        _ => RenameKey::Other,
    }
}
