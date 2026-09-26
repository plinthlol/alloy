// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// the outer frame for the content area: tab bar, keybind footer,
// and dispatching render calls to the active tab's widget.
// also renders the instance name/version header with run state indicators.

fn launch_key() -> &'static str {
    if cfg!(target_os = "macos") { "\u{2318}+\u{23ce}" } else { "ctrl+\u{23ce}" }
}

fn open_dir_key() -> &'static str {
    "ctrl+o"
}

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use throbber_widgets_tui::{Throbber, ThrobberState};

use crate::config::theme::{BORDER_STYLE, THEME};
use crate::tui::app::FocusedArea;

use crate::tui::widgets::styled_title;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ContentTab {
    #[default]
    Mods,
    ResourcePacks,
    Screenshots,
    Worlds,
    Logs,
    Settings,
}

impl ContentTab {
    const ALL: &'static [ContentTab] = &[
        ContentTab::Mods,
        ContentTab::ResourcePacks,
        ContentTab::Screenshots,
        ContentTab::Worlds,
        ContentTab::Logs,
        ContentTab::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ContentTab::Mods => "Mods",
            ContentTab::ResourcePacks => "Resource Packs",
            ContentTab::Screenshots => "Screenshots",
            ContentTab::Worlds => "Worlds",
            ContentTab::Logs => "Logs",
            ContentTab::Settings => "Settings",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let idx = self.index();
        Self::ALL[if idx == 0 {
            Self::ALL.len() - 1
        } else {
            idx - 1
        }]
    }
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    focused: FocusedArea,
    content: &mut super::ContentArea,
    instance: Option<&crate::instance::InstanceConfig>,
    instances_dir: &std::path::Path,
    picker: &ratatui_image::picker::Picker,
    throbber_state: &mut ThrobberState,
) {
    let tab = content.tab;
    let theme = THEME.as_ref();
    let is_focused = focused == FocusedArea::Content;

    let border_color = if is_focused {
        theme.accent()
    } else {
        theme.border()
    };

    let tab_titles: Vec<Span> = ContentTab::ALL
        .iter()
        .enumerate()
        .flat_map(|(i, t)| {
            let mut spans = Vec::new();
            if i > 0 {
                spans.push(Span::styled(
                    "\u{2022}",
                    Style::default().fg(theme.text_dim()),
                ));
            }
            if i == tab.index() {
                let style = Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::BOLD);
                spans.push(Span::styled(format!(" {} ", t.label()), style));
            } else {
                spans.push(Span::styled(
                    format!(" {} ", t.label()),
                    Style::default().fg(theme.text()),
                ));
            }
            spans
        })
        .collect();

    let search_line = match tab {
        ContentTab::Mods => content.mods.search.title_line(),
        ContentTab::ResourcePacks => content.resource_packs.search.title_line(),
        ContentTab::Worlds => content.worlds.search.title_line(),
        ContentTab::Screenshots => content.screenshots.search.title_line(),
        ContentTab::Logs => {
            if content.logs.viewer_focused {
                content.logs.viewer_search.title_line()
            } else {
                content.logs.search.title_line()
            }
        }
        ContentTab::Settings => None,
    };

    let mut block = Block::default()
        .title_top(Line::from(tab_titles))
        .borders(Borders::ALL)
        .border_type(BORDER_STYLE.to_border_type())
        .border_style(Style::default().fg(border_color));

    if let Some(sl) = search_line {
        block = block.title_top(sl);
    }

    // keybinds change depending on which tab is active and whether
    // the content panel or instances panel has focus
    let kb: Option<&[(&str, &str)]> = if is_focused {
        Some(match tab {
            ContentTab::Mods | ContentTab::ResourcePacks => &[
                (launch_key(), " launch"),
                ("Esc", " kill"),
                (open_dir_key(), " open dir"),
                ("j/k", " navigate"),
                ("⏎", " toggle"),
                ("b", " browse"),
                ("d", " delete"),
                ("h/l", " tabs"),
                ("/", " search"),
            ],
            ContentTab::Worlds => &[
                (launch_key(), " launch"),
                ("Esc", " kill"),
                (open_dir_key(), " open dir"),
                ("j/k", " navigate"),
                ("d", " delete"),
                ("h/l", " tabs"),
                ("/", " search"),
            ],
            ContentTab::Screenshots => &[
                (launch_key(), " launch"),
                ("Esc", " kill"),
                (open_dir_key(), " open dir"),
                ("shift+HJKL", " grid"),
                ("⏎", " open"),
                ("d", " delete"),
                ("h/l", " tabs"),
                ("/", " search"),
            ],
            ContentTab::Logs => {
                if content.logs.viewer_focused {
                    if content.logs.display_count() == 0 {
                        // nothing to scroll or search when the pane is empty,
                        // so surface the one thing that's actually useful here:
                        // launching the instance (or killing it, if it somehow
                        // started between renders).
                        &[
                            (launch_key(), " launch"),
                            ("Esc", " kill"),
                            (open_dir_key(), " open dir"),
                        ]
                    } else {
                        &[
                            (launch_key(), " launch"),
                            ("Esc", " kill"),
                            (open_dir_key(), " open dir"),
                            ("j/k", " scroll"),
                            ("g/G", " top/bottom"),
                            ("d", " delete"),
                            ("ctrl+c", " copy"),
                            ("Esc", " back"),
                            ("/", " search"),
                        ]
                    }
                } else {
                    &[
                        (launch_key(), " launch"),
                        ("Esc", " kill"),
                        (open_dir_key(), " open dir"),
                        ("j/k", " navigate"),
                        ("⏎", " view"),
                        ("d", " delete"),
                        ("ctrl+c", " copy"),
                        ("h/l", " tabs"),
                        ("/", " search"),
                    ]
                }
            }
            ContentTab::Settings => {
                if content.settings.is_editing() {
                    &[("⏎", " confirm"), ("Esc", " cancel")]
                } else {
                    &[
                        (launch_key(), " launch"),
                        ("Esc", " kill"),
                        (open_dir_key(), " open dir"),
                        ("⏎", " edit/pick/toggle"),
                        ("j/k", " navigate"),
                        ("h/l", " tabs"),
                    ]
                }
            }
        })
    } else if focused == FocusedArea::Instances {
        Some(&[
            (launch_key(), " launch"),
            ("Esc", " kill"),
            (open_dir_key(), " open dir"),
            ("⏎", " content"),
            ("a", " add"),
            ("d", " delete"),
            ("s", " settings"),
            ("/", " search"),
        ])
    } else {
        None
    };

    if let Some(kb) = kb {
        let lines =
            crate::tui::widgets::popups::keybind_lines_wrapped(kb, area.width.saturating_sub(2));
        for line in lines {
            block = block.title_bottom(line);
        }
    }

    let content_area = block.inner(area);
    frame.render_widget(block, area);

    // lazy-load: only scan when switching to an instance that hasn't been loaded yet
    match tab {
        ContentTab::Mods => {
            if let Some(instance) = instance {
                if content.mods.loaded_for.as_deref() != Some(instance.name.as_str()) {
                    let content_dir = instances_dir
                        .join(&instance.name)
                        .join(".minecraft")
                        .join("mods");
                    content.mods.start_load(
                        &content_dir,
                        &instance.name,
                        crate::instance::scan_one_mod,
                        ".jar",
                    );
                    content.mods.watch_dir(content_dir);
                }
                super::list::render(
                    frame,
                    content_area,
                    &mut content.mods,
                    is_focused,
                    "Loading mods...",
                    "No mods installed.",
                    picker,
                );
            } else {
                frame.render_widget(
                    Paragraph::new("No instance selected.")
                        .style(Style::default().fg(theme.text_dim())),
                    content_area,
                );
            }
        }
        ContentTab::ResourcePacks => {
            if let Some(instance) = instance {
                if content.resource_packs.loaded_for.as_deref() != Some(instance.name.as_str()) {
                    let content_dir = instances_dir
                        .join(&instance.name)
                        .join(".minecraft")
                        .join("resourcepacks");
                    content.resource_packs.start_load(
                        &content_dir,
                        &instance.name,
                        crate::instance::scan_one_resource_pack,
                        ".zip",
                    );
                    content.resource_packs.watch_dir(content_dir);
                }
                super::list::render(
                    frame,
                    content_area,
                    &mut content.resource_packs,
                    is_focused,
                    "Loading resource packs...",
                    "No resource packs installed.",
                    picker,
                );
            } else {
                frame.render_widget(
                    Paragraph::new("No instance selected.")
                        .style(Style::default().fg(theme.text_dim())),
                    content_area,
                );
            }
        }
        ContentTab::Logs => {
            if let Some(instance) = instance {
                if content.logs.loaded_for.as_deref() != Some(instance.name.as_str()) {
                    content.logs.start_load(instances_dir, &instance.name);
                }
                crate::tui::widgets::logs_viewer::render(
                    frame,
                    content_area,
                    &mut content.logs,
                    is_focused,
                );
            } else {
                frame.render_widget(
                    Paragraph::new("No instance selected.")
                        .style(Style::default().fg(theme.text_dim())),
                    content_area,
                );
            }
        }
        ContentTab::Screenshots => {
            if let Some(instance) = instance {
                if content.screenshots.loaded_for.as_deref() != Some(instance.name.as_str()) {
                    content.screenshots.start_load(instances_dir, &instance.name);
                }
                crate::tui::widgets::screenshots_grid::render(
                    frame,
                    content_area,
                    &mut content.screenshots,
                    is_focused,
                    throbber_state,
                );
            } else {
                frame.render_widget(
                    Paragraph::new("No instance selected.")
                        .style(Style::default().fg(theme.text_dim())),
                    content_area,
                );
            }
        }
        ContentTab::Worlds => {
            if let Some(instance) = instance {
                if content.worlds.loaded_for.as_deref() != Some(instance.name.as_str()) {
                    let content_dir = instances_dir
                        .join(&instance.name)
                        .join(".minecraft")
                        .join("saves");
                    content.worlds.start_load(
                        &content_dir,
                        &instance.name,
                        crate::instance::scan_one_world,
                        "",
                    );
                    content.worlds.watch_dir(content_dir);
                }
                super::list::render(
                    frame,
                    content_area,
                    &mut content.worlds,
                    is_focused,
                    "Loading worlds...",
                    "No worlds saved.",
                    picker,
                );
            } else {
                frame.render_widget(
                    Paragraph::new("No instance selected.")
                        .style(Style::default().fg(theme.text_dim())),
                    content_area,
                );
            }
        }
        ContentTab::Settings => {
            super::settings::render(frame, content_area, is_focused, &content.settings, instance);
        }
    }
}

// the header bar above the content tabs, showing instance name, loader info,
// and a spinner/error indicator when the instance is running or crashed
pub fn title(
    frame: &mut Frame,
    area: Rect,
    focused: FocusedArea,
    instance: Option<&crate::instance::InstanceConfig>,
    throbber_state: &mut ThrobberState,
    renaming: Option<&str>,
) {
    let theme = THEME.as_ref();
    let color = if focused == FocusedArea::Content {
        theme.accent()
    } else {
        theme.border()
    };

    let block = Block::default()
        .title(styled_title("Content", true))
        .borders(Borders::ALL)
        .border_type(BORDER_STYLE.to_border_type())
        .border_style(Style::default().fg(color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    match instance {
        None => {
            frame.render_widget(
                Paragraph::new("No instance selected").style(Style::default().fg(theme.text_dim())),
                inner,
            );
        }
        Some(inst) => {
            let [left_area, right_area] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(32)]).areas(inner);

            if let Some(rename_val) = renaming {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(rename_val, Style::default().fg(theme.text())),
                        Span::styled(
                            "\u{2588}",
                            Style::default()
                                .fg(theme.text_dim())
                                .add_modifier(Modifier::SLOW_BLINK),
                        ),
                    ])),
                    left_area,
                );
                let loader_str = match &inst.loader_version {
                    Some(lv) => format!("{} \u{00b7} {} {}", inst.game_version, inst.loader, lv),
                    None => format!("{} \u{00b7} {}", inst.game_version, inst.loader),
                };
                frame.render_widget(
                    Paragraph::new(loader_str)
                        .style(Style::default().fg(theme.text_dim()))
                        .alignment(Alignment::Right),
                    right_area,
                );
                return;
            }

            use crate::running::RunState;
            let run_state = crate::running::get(&inst.name);

            match run_state {
                Some(RunState::Authenticating)
                | Some(RunState::Running)
                | Some(RunState::Starting) => {
                    let throbber = Throbber::default()
                        .label(inst.name.as_str())
                        .style(
                            Style::default()
                                .fg(theme.text())
                                .add_modifier(Modifier::BOLD),
                        )
                        .throbber_style(
                            Style::default()
                                .fg(theme.success())
                                .add_modifier(Modifier::BOLD),
                        )
                        .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE)
                        .use_type(throbber_widgets_tui::WhichUse::Spin);
                    frame.render_stateful_widget(throbber, left_area, throbber_state);
                }
                Some(RunState::Crashed(_)) => {
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(
                                "\u{2717} ",
                                Style::default()
                                    .fg(theme.error())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                inst.name.as_str(),
                                Style::default()
                                    .fg(theme.text())
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ])),
                        left_area,
                    );
                }
                Some(RunState::Orphaned(_)) => {
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            inst.name.as_str(),
                            Style::default()
                                .fg(theme.text())
                                .add_modifier(Modifier::BOLD),
                        )),
                        left_area,
                    );
                }
                None => {
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            inst.name.as_str(),
                            Style::default()
                                .fg(theme.text())
                                .add_modifier(Modifier::BOLD),
                        )),
                        left_area,
                    );
                }
            }

            let loader_str = match &inst.loader_version {
                Some(lv) => format!("{} \u{00b7} {} {}", inst.game_version, inst.loader, lv),
                None => format!("{} \u{00b7} {}", inst.game_version, inst.loader),
            };
            frame.render_widget(
                Paragraph::new(loader_str)
                    .style(Style::default().fg(theme.text_dim()))
                    .alignment(Alignment::Right),
                right_area,
            );
        }
    }
}