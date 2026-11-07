use crate::app::{
    App, FocusTarget, LayoutGeometry, ModalState, NotebookPickerModal, PreferencesModal,
    PromptKind, SaveIndicator, TagEditorModal, TagModalRow, TextPromptModal,
};
use crate::domain::{LeftPanelWidthCols, MAX_LEFT_TITLES};
use crate::search::ParsedSearch;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

const ASCII_BORDER: border::Set<'static> = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

const SPINNER_FRAMES: &[&str] = &["|", "/", "-", "\\"];
const BAR_FRAMES: &[&str] = &[
    "[==    ]", "[ ==   ]", "[  ==  ]", "[   == ]", "[  ==  ]", "[ ==   ]",
];

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let mut geom = LayoutGeometry {
        terminal_width: area.width,
        ..LayoutGeometry::default()
    };

    let root_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);

    render_top_bar(frame, app, root_chunks[0], &mut geom);
    render_main_split(frame, app, root_chunks[1], &mut geom);
    render_footer_bar(frame, app, root_chunks[2]);

    render_modal(frame, app, area, &mut geom);

    app.geometry = geom;
    app.sync_vim_size();
}

fn render_top_bar(frame: &mut Frame<'_>, app: &App, area: Rect, geom: &mut LayoutGeometry) {
    let toggle_label = if app.left_panel_visible {
        " << Hide "
    } else {
        " >> Titles "
    };
    let toggle_w = u16::try_from(toggle_label.width()).unwrap_or(11);

    let status_spans = build_status_spans(app);
    let status_w = u16::try_from(
        status_spans
            .iter()
            .map(|s| s.content.width())
            .sum::<usize>()
            .saturating_add(1),
    )
    .unwrap_or(20)
    .min(area.width.saturating_sub(toggle_w + 10));

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(toggle_w),
            Constraint::Min(10),
            Constraint::Length(status_w),
        ])
        .split(area);

    geom.toggle_left_btn = cols[0];
    geom.search_bar = cols[1];

    let toggle_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(Span::styled(toggle_label, toggle_style)),
        cols[0],
    );

    let search_focused = app.focus == FocusTarget::SearchInput;
    let search_prefix = if search_focused {
        " / "
    } else {
        " Search (/): "
    };
    let display_query = if search_focused {
        &app.search_input.text
    } else if app.active_query.is_empty() {
        "tag:... notebook:... (t: / n: / t ... / n ...)"
    } else {
        &app.active_query
    };

    let bar_bg = if search_focused {
        Color::Blue
    } else {
        Color::Black
    };

    let query_style = if search_focused {
        Style::default()
            .fg(Color::White)
            .bg(bar_bg)
            .add_modifier(Modifier::BOLD)
    } else if app.active_query.is_empty() {
        Style::default().fg(Color::DarkGray).bg(bar_bg)
    } else {
        Style::default().fg(Color::Yellow).bg(bar_bg)
    };

    let mut search_spans = vec![
        Span::styled(
            search_prefix,
            Style::default()
                .fg(if search_focused {
                    Color::Yellow
                } else {
                    Color::Cyan
                })
                .bg(bar_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(display_query, query_style),
    ];

    if search_focused && !app.search_input.text.trim().is_empty() {
        let parsed = ParsedSearch::parse(&app.search_input.text);
        let mut chips = Vec::new();
        if let Some(nb) = &parsed.notebook {
            chips.push(format!("nb={nb}"));
        }
        for tg in &parsed.tags {
            let sign = if tg.negated { "-" } else { "#" };
            chips.push(format!("{sign}{}", tg.name));
        }
        if !chips.is_empty() {
            search_spans.push(Span::styled(
                format!("  ({})", chips.join(" ")),
                Style::default().fg(Color::Green).bg(bar_bg),
            ));
        }
    }

    let search_bar_widget =
        Paragraph::new(Line::from(search_spans)).style(Style::default().bg(bar_bg));
    frame.render_widget(search_bar_widget, cols[1]);

    if search_focused && matches!(app.modal, ModalState::None) {
        let prefix_w = u16::try_from(search_prefix.width()).unwrap_or(3);
        let cursor_prefix: String = app
            .search_input
            .text
            .chars()
            .take(app.search_input.cursor_chars)
            .collect();
        let cursor_x = cols[1]
            .x
            .saturating_add(prefix_w)
            .saturating_add(u16::try_from(cursor_prefix.width()).unwrap_or(0))
            .min(cols[1].x + cols[1].width.saturating_sub(1));
        frame.set_cursor_position((cursor_x, cols[1].y));
    }

    frame.render_widget(
        Paragraph::new(Line::from(status_spans)).style(Style::default().bg(Color::Black)),
        cols[2],
    );
}

fn build_status_spans(app: &App) -> Vec<Span<'static>> {
    match &app.save_indicator {
        SaveIndicator::Saving { label, .. } => {
            let sp = SPINNER_FRAMES[app.anim_tick % SPINNER_FRAMES.len()];
            let bar = BAR_FRAMES[(app.anim_tick / 2) % BAR_FRAMES.len()];
            vec![Span::styled(
                format!(" {sp} {label} {bar} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]
        }
        SaveIndicator::Saved { label, .. } => vec![Span::styled(
            format!(" [OK] {label} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )],
        SaveIndicator::Failed { message, .. } => vec![Span::styled(
            format!(" [ERR] {} ", truncate_ascii_dots(message, 36)),
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        )],
        SaveIndicator::Idle => {
            if let Some(err) = &app.status_error {
                vec![Span::styled(
                    format!(" [!] {} ", truncate_ascii_dots(err, 38)),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )]
            } else if app.loading_list {
                let sp = SPINNER_FRAMES[app.anim_tick % SPINNER_FRAMES.len()];
                vec![Span::styled(
                    format!(" {sp} Syncing... "),
                    Style::default().fg(Color::Cyan),
                )]
            } else {
                vec![Span::styled(
                    format!(" {}/{} notes ", app.notes.len(), app.total_matching),
                    Style::default().fg(Color::Gray),
                )]
            }
        }
    }
}

fn render_main_split(frame: &mut Frame<'_>, app: &mut App, area: Rect, geom: &mut LayoutGeometry) {
    geom.split_y_range = (area.y, area.y.saturating_add(area.height));

    if app.left_panel_visible {
        let clamped_left = LeftPanelWidthCols::new(app.left_width.get(), area.width);
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(clamped_left.get()),
                Constraint::Length(1),
                Constraint::Min(20),
            ])
            .split(area);

        render_left_titles_panel(frame, app, panes[0], geom);
        render_vertical_separator(frame, app, panes[1], geom);
        render_right_note_panel(frame, app, panes[2], geom);
    } else {
        geom.left_titles_inner = None;
        geom.separator_x = None;
        render_right_note_panel(frame, app, area, geom);
    }
}

fn render_left_titles_panel(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    geom: &mut LayoutGeometry,
) {
    let focused = app.focus == FocusTarget::LeftTitles;
    let border_color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let title = format!(" Notes ({}/{MAX_LEFT_TITLES}) ", app.notes.len());
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(if focused { Color::Cyan } else { Color::Gray })
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::TOP | Borders::BOTTOM | Borders::LEFT)
        .border_set(ASCII_BORDER)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    geom.left_titles_inner = Some(inner);
    frame.render_widget(block, area);

    if app.notes.is_empty() {
        let msg = if app.loading_list {
            "Loading latest notes..."
        } else {
            "No matching notes."
        };
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let max_title_cols = usize::from(inner.width);
    let items: Vec<ListItem<'_>> = app
        .notes
        .iter()
        .take(MAX_LEFT_TITLES)
        .map(|note| {
            let clean_title = if note.title.trim().is_empty() {
                "(Untitled)".to_owned()
            } else {
                truncate_ascii_dots(&note.title, max_title_cols)
            };
            ListItem::new(Line::from(Span::raw(clean_title)))
        })
        .collect();

    let highlight_style = if focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::White)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    };

    let mut state = ListState::default();
    state.select(Some(app.selected_index));
    let list = List::new(items).highlight_style(highlight_style);
    frame.render_stateful_widget(list, inner, &mut state);
}

fn render_vertical_separator(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    geom: &mut LayoutGeometry,
) {
    geom.separator_x = Some(area.x);
    let glyph = if app.dragging_separator { "#" } else { "|" };
    let style = if app.dragging_separator {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if app.focus == FocusTarget::LeftTitles || app.focus == FocusTarget::RightVim {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let buf = frame.buffer_mut();
    let top = area.y;
    let bottom = area.y.saturating_add(area.height.saturating_sub(1));
    let mid = area.y + area.height / 2;
    for y in area.y..area.y.saturating_add(area.height) {
        let sym = if y == top || y == bottom || y == mid {
            "+"
        } else {
            glyph
        };
        buf[(area.x, y)].set_symbol(sym).set_style(style);
    }
}

fn render_right_note_panel(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    geom: &mut LayoutGeometry,
) {
    let focused = app.focus == FocusTarget::RightVim;
    let border_color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let borders = if app.left_panel_visible {
        Borders::BOTTOM | Borders::RIGHT
    } else {
        Borders::BOTTOM | Borders::LEFT | Borders::RIGHT
    };

    let block = Block::default()
        .borders(borders)
        .border_set(ASCII_BORDER)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 5 || inner.width < 10 {
        geom.vim_area = inner;
        return;
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(2)])
        .split(inner);

    render_note_metadata_header(frame, app, sections[0], geom);
    geom.vim_area = sections[1];

    if let Some(vim) = &mut app.vim_session {
        vim.resize(sections[1].height, sections[1].width);
        vim.drain_output();
    }

    if let Some(vim) = &app.vim_session {
        let cursor = vim.render(sections[1], frame.buffer_mut());
        if focused
            && matches!(app.modal, ModalState::None)
            && let Some((cx, cy)) = cursor
        {
            frame.set_cursor_position((cx, cy));
        }
    } else if let Some(preview_md) = app.selected_preview_text() {
        let lines: Vec<Line<'_>> = if preview_md.is_empty() {
            vec![Line::from(Span::styled(
                " (Empty note - press Enter to edit in Vim)",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            preview_md
                .lines()
                .map(|line| {
                    let trimmed = line.trim();
                    if trimmed.starts_with("[Image: ") || trimmed.starts_with("[Attachment: ") {
                        Line::from(Span::styled(
                            format!(" {line}"),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ))
                    } else if trimmed.starts_with('#') {
                        Line::from(Span::styled(
                            format!(" {line}"),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ))
                    } else {
                        Line::from(Span::styled(
                            format!(" {line}"),
                            Style::default().fg(Color::White),
                        ))
                    }
                })
                .collect()
        };
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((app.preview_scroll, 0)),
            sections[1],
        );
    } else if app.loading_note_guid.is_some() {
        let sp = SPINNER_FRAMES[app.anim_tick % SPINNER_FRAMES.len()];
        frame.render_widget(
            Paragraph::new(format!(" {sp} Loading note content..."))
                .style(Style::default().fg(Color::Cyan)),
            sections[1],
        );
    } else {
        frame.render_widget(
            Paragraph::new(" Select a note on the left or press 'c' to create a new note.")
                .style(Style::default().fg(Color::DarkGray)),
            sections[1],
        );
    }
}

fn render_note_metadata_header(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    geom: &mut LayoutGeometry,
) {
    let Some(summary) = app.selected_summary() else {
        frame.render_widget(
            Paragraph::new(" No note selected").style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    };

    let detail = app.selected_detail();

    // Row 0: Title (clickable to rename)
    let title_str = detail.map_or(summary.title.as_str(), |d| d.title.as_str());
    geom.title_hit = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let line_title = Line::from(vec![
        Span::styled(" Title: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            truncate_ascii_dots(title_str, usize::from(area.width.saturating_sub(18))),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  [r]", Style::default().fg(Color::DarkGray)),
    ]);

    // Row 1: Notebook + Tags (clickable ASCII badges)
    let nb_guid = detail
        .and_then(|d| d.notebook_guid.as_ref())
        .or(summary.notebook_guid.as_ref());
    let nb_name = app.notebook_name(nb_guid);
    let nb_badge = format!(" [Notebook: {nb_name} (n)] ");
    let nb_w = u16::try_from(nb_badge.width())
        .unwrap_or(16)
        .min(area.width);
    geom.notebook_hit = Rect {
        x: area.x,
        y: area.y + 1,
        width: nb_w,
        height: 1,
    };

    let tags_display = match detail {
        Some(d) if !d.tag_names.is_empty() => d
            .tag_names
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(" "),
        Some(_) => "(none)".to_owned(),
        None if !summary.tag_guids.is_empty() => format!("{} tag(s)", summary.tag_guids.len()),
        None => "(none)".to_owned(),
    };
    let tags_badge = format!("[Tags: {tags_display} + (t)]");
    geom.tags_hit = Rect {
        x: area.x.saturating_add(nb_w + 1).min(area.x + area.width),
        y: area.y + 1,
        width: area.width.saturating_sub(nb_w + 1),
        height: 1,
    };

    let line_nb_tags = Line::from(vec![
        Span::styled(
            nb_badge,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            truncate_ascii_dots(
                &tags_badge,
                usize::from(area.width.saturating_sub(nb_w + 2)),
            ),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    // Row 2: Created | Updated | Attachments | Size | URL
    let created = detail
        .and_then(|d| d.created)
        .or(summary.created)
        .map_or_else(|| "-".to_owned(), |ts| ts.format_datetime());
    let updated = detail
        .and_then(|d| d.updated)
        .or(summary.updated)
        .map_or_else(|| "-".to_owned(), |ts| ts.format_datetime());

    let mut meta_extra = Vec::new();
    if let Some(d) = detail {
        let imgs = d.image_count();
        let files = d.attachment_count();
        if imgs > 0 {
            meta_extra.push(format!("{imgs} img"));
        }
        if files > 0 {
            meta_extra.push(format!("{files} file"));
        }
        if let Some(len) = d.content_length.or(summary.content_length) {
            meta_extra.push(format_bytes(len));
        }
        if let Some(url) = &d.source_url {
            meta_extra.push(truncate_ascii_dots(url, 28));
        }
    }
    let extra_str = if meta_extra.is_empty() {
        String::new()
    } else {
        format!("  |  {}", meta_extra.join(" | "))
    };

    let line_dates = Line::from(vec![
        Span::styled(" Created: ", Style::default().fg(Color::DarkGray)),
        Span::styled(created, Style::default().fg(Color::Gray)),
        Span::styled("  |  Updated: ", Style::default().fg(Color::DarkGray)),
        Span::styled(updated, Style::default().fg(Color::Gray)),
        Span::styled(extra_str, Style::default().fg(Color::DarkGray)),
    ]);

    // Row 3: Pure ASCII horizontal rule
    let rule = "-".repeat(usize::from(area.width));
    let line_rule = Line::from(Span::styled(rule, Style::default().fg(Color::DarkGray)));

    frame.render_widget(
        Paragraph::new(vec![line_title, line_nb_tags, line_dates, line_rule]),
        area,
    );
}

fn render_footer_bar(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let hints = match (&app.modal, app.focus) {
        (ModalState::TagEditor(_), _) => {
            " Space:Toggle tag | Type:Filter / new tag | Enter:Save tags | Esc:Cancel"
        }
        (ModalState::NotebookPicker(_), _) => {
            " Up/Down:Select | Type:Filter notebooks | Enter:Set notebook | Esc:Cancel"
        }
        (ModalState::TextPrompt(_), _) => " Enter:Confirm | Ctrl+U:Clear | Esc:Cancel",
        (ModalState::Preferences(_), _) => {
            " Space/Enter:Toggle checkbox | Up/Down:Select | Esc/p/q:Close preferences"
        }
        (ModalState::Help, _) => " Esc / Enter / q:Close help",
        (ModalState::None, FocusTarget::LeftTitles) => {
            " j/k/1-9:Select  Enter:Edit in Vim  /:Search  t:Tags  n:Notebook  r:Rename  c:New  p:Prefs  b:Hide  ?:Help  q:Quit"
        }
        (ModalState::None, FocusTarget::SearchInput) => {
            " Enter:Run search | Syntax: tag:x  notebook:y  t:x  n:y  \"t x\"  \"n y\" | Ctrl+U:Clear | Esc:Back"
        }
        (ModalState::None, FocusTarget::RightVim) if app.vim_session.is_none() => {
            " j/k/PgUp/PgDn:Scroll  1-9:Note  Enter:Edit in Vim  p:Prefs  Esc/Tab/h:Titles  t:Tags  n:Notebook  q:Quit"
        }
        (ModalState::None, FocusTarget::RightVim) => {
            " :w:Save to Evernote | :q:Close Vim | Ctrl+\\ / Alt+h:Titles | F2:Toggle Left | F4:Tags | F5:Notebook | F7:Prefs"
        }
    };

    let bar = Paragraph::new(Span::styled(
        truncate_ascii_dots(hints, usize::from(area.width)),
        Style::default().fg(Color::Gray).bg(Color::Black),
    ))
    .style(Style::default().bg(Color::Black));
    frame.render_widget(bar, area);
}

fn render_modal(frame: &mut Frame<'_>, app: &App, area: Rect, geom: &mut LayoutGeometry) {
    match &app.modal {
        ModalState::None => geom.modal_inner = None,
        ModalState::TagEditor(tm) => render_tag_modal(frame, app, tm, area, geom),
        ModalState::NotebookPicker(nm) => render_notebook_modal(frame, app, nm, area, geom),
        ModalState::TextPrompt(pm) => render_prompt_modal(frame, pm, area, geom),
        ModalState::Preferences(pm) => render_preferences_modal(frame, app, pm, area, geom),
        ModalState::Help => render_help_modal(frame, area, geom),
    }
}

fn render_tag_modal(
    frame: &mut Frame<'_>,
    app: &App,
    tm: &TagEditorModal,
    area: Rect,
    geom: &mut LayoutGeometry,
) {
    let popup = centered_rect(area, 62, 18);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(
            format!(
                " Manage Tags - {} ",
                truncate_ascii_dots(&tm.note_title, 34)
            ),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_set(ASCII_BORDER)
        .border_style(Style::default().fg(Color::Green))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup);
    geom.modal_inner = Some(inner);
    frame.render_widget(block, popup);

    if inner.height < 4 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let active_summary = if tm.selected_tags.is_empty() {
        "Active: (none)".to_owned()
    } else {
        format!(
            "Active: {}",
            tm.selected_tags
                .iter()
                .map(|t| format!("#{t}"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    frame.render_widget(
        Paragraph::new(truncate_ascii_dots(
            &active_summary,
            usize::from(chunks[0].width),
        ))
        .style(Style::default().fg(Color::Green)),
        chunks[0],
    );

    let prompt = "Filter / New tag: ";
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Cyan)),
            Span::styled(&tm.filter.text, Style::default().fg(Color::White)),
        ])),
        chunks[1],
    );
    set_input_cursor(
        frame,
        chunks[1],
        prompt,
        &tm.filter.text,
        tm.filter.cursor_chars,
    );

    frame.render_widget(
        Paragraph::new("-".repeat(usize::from(chunks[2].width)))
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    let rows = tm.visible_rows(&app.tags);
    let items: Vec<ListItem<'_>> = rows
        .iter()
        .map(|row| match row {
            TagModalRow::CreateNew(name) => ListItem::new(Line::from(vec![
                Span::styled(" [+] ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("Create & add tag \"{name}\""),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            TagModalRow::Existing { name, checked } => {
                let box_span = if *checked {
                    Span::styled(" [x] ", Style::default().fg(Color::Green))
                } else {
                    Span::styled(" [ ] ", Style::default().fg(Color::DarkGray))
                };
                ListItem::new(Line::from(vec![box_span, Span::raw(name.clone())]))
            }
        })
        .collect();

    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(tm.cursor.min(items.len() - 1)));
    }
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, chunks[3], &mut state);
}

fn render_notebook_modal(
    frame: &mut Frame<'_>,
    app: &App,
    nm: &NotebookPickerModal,
    area: Rect,
    geom: &mut LayoutGeometry,
) {
    let popup = centered_rect(area, 58, 18);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(
            format!(
                " Set Notebook - {} ",
                truncate_ascii_dots(&nm.note_title, 32)
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_set(ASCII_BORDER)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup);
    geom.modal_inner = Some(inner);
    frame.render_widget(block, popup);

    if inner.height < 4 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let cur_name = app.notebook_name(nm.current_notebook.as_ref());
    frame.render_widget(
        Paragraph::new(format!("Current: {cur_name}")).style(Style::default().fg(Color::Gray)),
        chunks[0],
    );

    let prompt = "Filter: ";
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Cyan)),
            Span::styled(&nm.filter.text, Style::default().fg(Color::White)),
        ])),
        chunks[1],
    );
    set_input_cursor(
        frame,
        chunks[1],
        prompt,
        &nm.filter.text,
        nm.filter.cursor_chars,
    );

    frame.render_widget(
        Paragraph::new("-".repeat(usize::from(chunks[2].width)))
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    let filtered = nm.filtered(&app.notebooks);
    let items: Vec<ListItem<'_>> = filtered
        .iter()
        .map(|nb| {
            let is_cur = nm.current_notebook.as_ref() == Some(&nb.guid);
            let marker = if is_cur { " * " } else { "   " };
            let label = match &nb.stack {
                Some(s) if !s.is_empty() => format!("{s} / {}", nb.name),
                _ => nb.name.clone(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if is_cur {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::raw(label),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(nm.cursor.min(items.len() - 1)));
    }
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, chunks[3], &mut state);
}

fn render_prompt_modal(
    frame: &mut Frame<'_>,
    pm: &TextPromptModal,
    area: Rect,
    geom: &mut LayoutGeometry,
) {
    let popup = centered_rect(area, 56, 5);
    frame.render_widget(Clear, popup);

    let title = match pm.kind {
        PromptKind::RenameNote => " Rename Note Title ",
        PromptKind::CreateNote => " Create New Note ",
    };

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_set(ASCII_BORDER)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup);
    geom.modal_inner = Some(inner);
    frame.render_widget(block, popup);

    if inner.height >= 2 {
        let row = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: 1,
        };
        let prompt = "Title: ";
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prompt, Style::default().fg(Color::Cyan)),
                Span::styled(&pm.input.text, Style::default().fg(Color::White)),
            ])),
            row,
        );
        set_input_cursor(frame, row, prompt, &pm.input.text, pm.input.cursor_chars);
    }
}

fn render_preferences_modal(
    frame: &mut Frame<'_>,
    app: &App,
    pm: &PreferencesModal,
    area: Rect,
    geom: &mut LayoutGeometry,
) {
    let popup = centered_rect(area, 68, 10);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(
            " Preferences (p) ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_set(ASCII_BORDER)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup);
    geom.modal_inner = Some(inner);
    frame.render_widget(block, popup);

    if inner.height < 4 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new("Space / Enter toggles option (saved automatically):")
            .style(Style::default().fg(Color::Gray)),
        chunks[0],
    );

    let checked_span = if app.preferences.vim_preview {
        Span::styled(" [x] ", Style::default().fg(Color::Green))
    } else {
        Span::styled(" [ ] ", Style::default().fg(Color::DarkGray))
    };
    let items = vec![ListItem::new(Line::from(vec![
        checked_span,
        Span::raw("Vim preview"),
    ]))];
    let mut state = ListState::default();
    state.select(Some(pm.cursor.min(items.len().saturating_sub(1))));
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, chunks[1], &mut state);

    frame.render_widget(
        Paragraph::new("-".repeat(usize::from(chunks[2].width)))
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    let desc = vec![
        Line::from(Span::styled(
            "Unchecked (default): fast text preview; press Enter to edit in Vim.",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "Checked: open every selected note directly in embedded Vim.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(desc), chunks[3]);
}

fn render_help_modal(frame: &mut Frame<'_>, area: Rect, geom: &mut LayoutGeometry) {
    let popup = centered_rect(area, 72, 22);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(
            " evertui Help ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_set(ASCII_BORDER)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup);
    geom.modal_inner = Some(inner);
    frame.render_widget(block, popup);

    let lines = vec![
        Line::from(Span::styled(
            "Search Syntax (/ or F3 / Alt+s):",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  * tag:work   t:work   t work   -t:archive   t:\"two words\""),
        Line::from("  * notebook:INBOX   n:INBOX   n INBOX   n \"Work Notes\""),
        Line::from(""),
        Line::from(Span::styled(
            "Left Panel (Titles, max 20):",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  * j / k / 1-9 / Click        Select note (fast text preview)"),
        Line::from("  * Enter / e / i              Replace preview with embedded Vim"),
        Line::from("  * Tab / l / PgUp / PgDn      Focus or scroll text preview"),
        Line::from("  * p (or F7 / Alt+p)          Preferences (\"Vim preview\" toggle)"),
        Line::from("  * t (or F4 / Alt+t)          Add / remove tags on note"),
        Line::from("  * n (or F5 / Alt+n)          Change note's notebook"),
        Line::from("  * r (or F6 / Alt+r)          Rename note title"),
        Line::from("  * c                          Create new note"),
        Line::from("  * b (or F2 / Alt+b)          Hide / show left panel"),
        Line::from(""),
        Line::from(Span::styled(
            "Right Panel (Real Embedded Vim + Mouse):",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  * :w                         Save buffer directly to Evernote"),
        Line::from("  * :q / :wq                   Close Vim & return to preview"),
        Line::from("  * Ctrl+\\ or Alt+h            Switch focus to Left Panel"),
        Line::from("  * Drag vertical | border     Resize panels live with mouse"),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn set_input_cursor(
    frame: &mut Frame<'_>,
    area: Rect,
    prefix: &str,
    text: &str,
    cursor_chars: usize,
) {
    let prefix_w = u16::try_from(prefix.width()).unwrap_or(0);
    let before: String = text.chars().take(cursor_chars).collect();
    let x = area
        .x
        .saturating_add(prefix_w)
        .saturating_add(u16::try_from(before.width()).unwrap_or(0))
        .min(area.x + area.width.saturating_sub(1));
    frame.set_cursor_position((x, area.y));
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2)).max(16);
    let h = height.min(area.height.saturating_sub(2)).max(4);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn truncate_ascii_dots(input: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if input.width() <= max_width {
        return input.to_owned();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let mut out = String::new();
    let mut used = 0usize;
    let budget = max_width - 3;
    for ch in input.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push_str("...");
    out
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NoteSummary, Notebook, NotebookGuid, SearchPage, Tag, TagGuid};
    use crate::worker::WorkerEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn renders_strictly_simple_ascii_and_ansi16_colors_for_tty() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.handle_worker_event(WorkerEvent::Bootstrapped {
            notebooks: vec![Notebook {
                guid: NotebookGuid::from("nb1"),
                name: "INBOX".to_owned(),
                stack: None,
                is_default: true,
            }],
            tags: vec![Tag {
                guid: TagGuid::from("t1"),
                name: "tui".to_owned(),
            }],
            page: SearchPage {
                total_matching: 1,
                notes: vec![NoteSummary {
                    guid: "g1".into(),
                    title: "ASCII Note".to_owned(),
                    created: None,
                    updated: None,
                    notebook_guid: Some(NotebookGuid::from("nb1")),
                    tag_guids: vec![TagGuid::from("t1")],
                    content_length: Some(128),
                }],
            },
        });
        app.save_indicator = SaveIndicator::Saving {
            label: "Saving note".to_owned(),
            started_at: Instant::now(),
            pending_completion: None,
        };
        app.modal = ModalState::Help;

        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        assert_pure_ascii_and_ansi16(terminal.backend().buffer());

        // Also verify Preferences modal renders strictly pure ASCII and ANSI-16 colors.
        app.open_preferences_modal();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert_pure_ascii_and_ansi16(terminal.backend().buffer());
    }

    #[test]
    fn left_panel_renders_titles_without_index_numbers() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.handle_worker_event(WorkerEvent::Bootstrapped {
            notebooks: Vec::new(),
            tags: Vec::new(),
            page: SearchPage {
                total_matching: 2,
                notes: vec![
                    NoteSummary {
                        guid: "g1".into(),
                        title: "Alpha title".to_owned(),
                        created: None,
                        updated: None,
                        notebook_guid: None,
                        tag_guids: Vec::new(),
                        content_length: None,
                    },
                    NoteSummary {
                        guid: "g2".into(),
                        title: "Beta title".to_owned(),
                        created: None,
                        updated: None,
                        notebook_guid: None,
                        tag_guids: Vec::new(),
                        content_length: None,
                    },
                ],
            },
        });

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let inner = app.geometry.left_titles_inner.expect("left panel visible");
        let buf = terminal.backend().buffer();
        let row0: String = (inner.x..inner.x + inner.width)
            .map(|x| buf[(x, inner.y)].symbol())
            .collect();
        let row1: String = (inner.x..inner.x + inner.width)
            .map(|x| buf[(x, inner.y + 1)].symbol())
            .collect();
        assert!(row0.starts_with("Alpha title"), "unexpected row0: {row0:?}");
        assert!(row1.starts_with("Beta title"), "unexpected row1: {row1:?}");
    }

    fn assert_pure_ascii_and_ansi16(buf: &ratatui::buffer::Buffer) {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                assert!(
                    cell.symbol().is_ascii(),
                    "Non-ASCII symbol {:?} found at ({x}, {y})",
                    cell.symbol()
                );
                assert!(
                    !matches!(cell.fg, Color::Rgb(_, _, _))
                        && !matches!(cell.bg, Color::Rgb(_, _, _)),
                    "24-bit RGB color used at ({x}, {y}) which breaks Linux TTY"
                );
            }
        }
    }
}
