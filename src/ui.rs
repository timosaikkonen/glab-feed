use chrono::Utc;
use chrono_humanize::HumanTime;
use ratatui::{
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, Padding, Paragraph, Row, Table, Tabs, Wrap,
    },
    Frame,
};

/// Secondary text on the second line of two-line MR rows (author, last comment).
const MR_SECONDARY: Color = Color::DarkGray;

// Nerd Font glyphs: Font Awesome (check, times, …) and Codicons (git_branch_conflicts).
const NF_CHECK: &str = "\u{f00c}";
const NF_TIMES: &str = "\u{f00d}";
const NF_REFRESH: &str = "\u{f021}";
const NF_BAN: &str = "\u{f05e}";
const NF_BUILD: &str = "\u{f085}";
const NF_NOTE: &str = "\u{f075}";
const NF_PUSH: &str = "\u{f126}";
const NF_GIT_BRANCH_CONFLICTS: &str = "\u{ec6e}";

fn update_activity_glyph(activity: UpdateActivity) -> &'static str {
    match activity {
        UpdateActivity::Build => NF_BUILD,
        UpdateActivity::Note => NF_NOTE,
        UpdateActivity::Push => NF_PUSH,
    }
}

use crate::app::{App, CopyChoice};
use crate::gitlab::{CiStatus, MergeRequest, UpdateActivity};
use crate::notifications::NotificationKind;
use crate::selector::DisplayRow;

pub fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(3), // tabs
        Constraint::Length(1), // MR filter (blank on notifications tab)
        Constraint::Min(1),    // table or notifications list
        Constraint::Length(2), // footer
    ])
    .split(f.area());

    render_tabs(f, app, chunks[0]);
    if app.is_notifications_tab() {
        f.render_widget(Paragraph::new(""), chunks[1]);
        render_notifications_list(f, app, chunks[2]);
    } else {
        render_mr_search(f, app, chunks[1]);
        render_table(f, app, chunks[2]);
    }
    render_footer(f, app, chunks[3]);

    if app.awaiting_url {
        render_url_prompt(f, app);
    } else if app.selector.is_some() {
        render_selector(f, app);
    } else if app.author_selector.is_some() {
        render_author_selector(f, app);
    } else if app.show_help {
        render_help(f, app);
    } else if app.copy_menu.is_some() {
        render_copy_menu(f, app);
    }
}

fn render_url_prompt(f: &mut Frame, app: &App) {
    let area = centered_rect(f.area(), 70, 40);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" First-run setup ")
        .title_bottom(" Enter continue  Esc quit ")
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from("Enter your GitLab remote URL or hostname:"),
        Line::from(Span::styled(
            "e.g. git@gitlab.example.com:group/repo.git  or  https://gitlab.example.com",
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::styled(
                app.url_input.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ]),
    ];
    if let Some(err) = &app.url_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut titles: Vec<Line> = app
        .repos
        .iter()
        .enumerate()
        .map(|(i, repo)| {
            let count = app.repo_states.get(i).map(|s| s.mrs.len()).unwrap_or(0);
            let err = app
                .repo_states
                .get(i)
                .map(|s| s.error.is_some())
                .unwrap_or(false);
            let mark = if err { " !" } else { "" };
            Line::from(format!(" {} ({}){} ", repo.label(), count, mark))
        })
        .collect();

    let notif_count = app.notifications().len();
    let mut notif_label = format!(" Notifications ({notif_count}) ");
    if app.new_notification_count > 0 && !app.is_notifications_tab() {
        notif_label = format!(" Notifications ({notif_count})* ");
    }
    titles.push(Line::from(notif_label));

    let tabs = Tabs::new(titles)
        .select(app.selected_tab)
        .block(Block::default().borders(Borders::ALL).title(" Tabs "))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("|");
    f.render_widget(tabs, area);
}

fn render_table(f: &mut Frame, app: &mut App, area: Rect) {
    let mrs = app.visible_mrs();
    let stale = app.showing_cached_mrs();
    let fetch_error = app
        .current_state()
        .and_then(|s| s.error.as_deref())
        .filter(|_| mrs.is_empty());

    let title = if stale {
        format!(" Merge Requests ({}) — offline ", mrs.len())
    } else if mrs.is_empty() {
        " Merge Requests ".to_string()
    } else {
        format!(" Merge Requests ({}) ", mrs.len())
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .padding(Padding::horizontal(1));
    if stale {
        block = block.title_bottom(" showing cached data ");
    }

    // Surface loading / empty-with-error states without replacing a populated table.
    if let Some(state) = app.current_state() {
        if !state.loaded {
            let p = Paragraph::new("Loading...").dim().block(block);
            f.render_widget(p, area);
            return;
        }
    }

    if mrs.is_empty() {
        if let Some(err) = fetch_error {
            let p = Paragraph::new(format!("Could not fetch merge requests.\n{err}"))
                .wrap(Wrap { trim: true })
                .red()
                .block(block);
            f.render_widget(p, area);
            return;
        }
        let msg = if !app.mr_query.is_empty() {
            format!("No merge requests matching \"{}\".", app.mr_query)
        } else if !app.author_filter.is_empty() {
            "No merge requests from selected authors.".to_string()
        } else {
            "No open merge requests.".to_string()
        };
        let p = Paragraph::new(msg).dim().block(block);
        f.render_widget(p, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("Title"),
        Cell::from("Comments"),
        Cell::from("Status"),
        Cell::from("CI"),
        Cell::from("Age"),
        Cell::from("Updated"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1);

    let selected = app.table_state.selected();
    let rows: Vec<Row> = mrs
        .iter()
        .enumerate()
        .map(|(i, mr)| build_row(mr, selected == Some(i)))
        .collect();
    drop(mrs); // Row owns its content; release the immutable borrow on `app`.

    let widths = [
        Constraint::Percentage(45),
        Constraint::Percentage(18),
        Constraint::Length(10),
        Constraint::Length(4),
        Constraint::Length(14),
        Constraint::Length(18),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn mr_title_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn mr_secondary_style(selected: bool) -> Style {
    if selected {
        mr_title_style()
    } else {
        Style::default().fg(MR_SECONDARY)
    }
}

fn build_row(mr: &MergeRequest, selected: bool) -> Row<'static> {
    let secondary = mr_secondary_style(selected);
    // Title cell: title + author underneath.
    let author = if mr.author_name.is_empty() {
        format!("@{}", mr.author_username)
    } else {
        format!("{} (@{})", mr.author_name, mr.author_username)
    };
    let title_cell = Cell::from(Text::from(vec![
        Line::from(Span::styled(
            format!("!{} {}", mr.iid, mr.title),
            mr_title_style(),
        )),
        Line::from(Span::styled(author, secondary)),
    ]));

    // Comments cell: count + last commenter/time.
    let second = match (&mr.last_note_author, mr.last_note_at) {
        (Some(who), Some(when)) => {
            let ago = HumanTime::from(when - Utc::now());
            format!("@{who} {ago}")
        }
        _ => "no comments".to_string(),
    };
    let comments_cell = Cell::from(Text::from(vec![
        Line::from(format!("{}", mr.notes_count)),
        Line::from(Span::styled(second, secondary)),
    ]));

    // Status cell.
    let (status_text, status_color) = if mr.draft {
        ("Draft", Color::Yellow)
    } else if mr.approved {
        ("Approved", Color::Green)
    } else {
        ("Open", Color::Cyan)
    };
    let mut status_spans = vec![Span::styled(status_text, Style::default().fg(status_color))];
    if mr.has_conflicts {
        status_spans.push(Span::raw(" "));
        status_spans.push(Span::styled(
            NF_GIT_BRANCH_CONFLICTS,
            Style::default().fg(Color::Red),
        ));
    }
    let status_cell = Cell::from(Text::from(vec![Line::from(status_spans)]));

    // CI cell.
    let (ci_text, ci_color) = match &mr.ci {
        CiStatus::Pass => (NF_CHECK.to_string(), Color::Green),
        CiStatus::Fail => (NF_TIMES.to_string(), Color::Red),
        CiStatus::InProgress => (NF_REFRESH.to_string(), Color::Yellow),
        CiStatus::Cancelled => (NF_BAN.to_string(), Color::DarkGray),
        CiStatus::Other(s) => (s.to_lowercase(), Color::Gray),
        CiStatus::None => ("-".to_string(), Color::Gray),
    };
    let ci_cell = Cell::from(Text::from(vec![Line::from(Span::styled(
        ci_text,
        Style::default().fg(ci_color),
    ))]));

    // Age cell: humanized time since the MR was created.
    let age = HumanTime::from(mr.created_at - Utc::now()).to_string();
    let age_cell = Cell::from(Text::from(vec![Line::from(Span::styled(
        age,
        Style::default().fg(Color::Gray),
    ))]));

    // Updated cell: humanized time + latest activity author underneath.
    let updated = HumanTime::from(mr.updated_at - Utc::now()).to_string();
    let mut updated_lines = vec![Line::from(Span::styled(
        updated,
        Style::default().fg(Color::Gray),
    ))];
    if let Some((activity, user)) = &mr.last_update {
        // Nerd Font icons are often two cells wide; pad with an extra space so the
        // gap before @username actually renders.
        updated_lines.push(Line::from(vec![
            Span::styled(update_activity_glyph(*activity), secondary),
            Span::styled("  ", secondary),
            Span::styled(user.clone(), secondary),
        ]));
    }
    let updated_cell = Cell::from(Text::from(updated_lines));

    Row::new(vec![
        title_cell,
        comments_cell,
        status_cell,
        ci_cell,
        age_cell,
        updated_cell,
    ])
    .height(2)
}

fn render_mr_search(f: &mut Frame, app: &App, area: Rect) {
    let line = if app.mr_search_active {
        Line::from(vec![
            Span::styled("/ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                app.mr_query.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ])
    } else if !app.mr_query.is_empty() {
        Line::from(Span::styled(
            format!("/ {}  ({} shown)", app.mr_query, app.visible_mrs().len()),
            Style::default().fg(Color::Gray),
        ))
    } else {
        Line::from(Span::styled(
            "/ filter by title or IID",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn notification_kind_glyph(kind: NotificationKind) -> (&'static str, Color) {
    match kind {
        NotificationKind::Comment | NotificationKind::CommentReply => (NF_NOTE, Color::Cyan),
        NotificationKind::ReviewSubmitted => (NF_CHECK, Color::Green),
        NotificationKind::ReviewRequested => (NF_REFRESH, Color::Yellow),
        NotificationKind::PipelineFailed => (NF_TIMES, Color::Red),
    }
}

fn render_notifications_list(f: &mut Frame, app: &mut App, area: Rect) {
    let items = app.notifications();
    let title = if items.is_empty() {
        " Notifications ".to_string()
    } else {
        format!(" Notifications ({}) ", items.len())
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .padding(Padding::horizontal(1));

    if items.is_empty() {
        let p = Paragraph::new("No notifications yet.").dim().block(block);
        f.render_widget(p, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("Activity"),
        Cell::from("Summary"),
        Cell::from("When"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1);

    let selected = app.notification_list_state.selected();
    let rows: Vec<Row> = items
        .iter()
        .enumerate()
        .map(|(i, n)| build_notification_row(n, selected == Some(i)))
        .collect();

    let widths = [
        Constraint::Length(3),
        Constraint::Percentage(30),
        Constraint::Percentage(50),
        Constraint::Length(14),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    f.render_stateful_widget(table, area, &mut app.notification_list_state);
}

fn build_notification_row(n: &crate::notifications::Notification, selected: bool) -> Row<'static> {
    let secondary = mr_secondary_style(selected);
    let (glyph, color) = notification_kind_glyph(n.kind);
    let kind_cell = Cell::from(Text::from(vec![Line::from(Span::styled(
        glyph,
        Style::default().fg(color),
    ))]));

    let mr_ref = n
        .mr_iid
        .as_ref()
        .map(|iid| format!("!{iid} "))
        .unwrap_or_default();
    let repo_line = if n.repo.is_empty() {
        mr_ref.trim().to_string()
    } else {
        format!("{} {}", n.repo, mr_ref).trim().to_string()
    };

    let activity_cell = Cell::from(Text::from(vec![
        Line::from(Span::styled(
            format!("@{} {}", n.author, n.kind.verb()),
            mr_title_style(),
        )),
        Line::from(Span::styled(repo_line, secondary)),
    ]));

    let summary_cell = Cell::from(Text::from(vec![Line::from(Span::styled(
        n.summary.clone(),
        secondary,
    ))]));

    let when = HumanTime::from(n.at - Utc::now()).to_string();
    let when_cell = Cell::from(Text::from(vec![Line::from(Span::styled(
        when,
        Style::default().fg(Color::Gray),
    ))]));

    Row::new(vec![kind_cell, activity_cell, summary_cell, when_cell]).height(2)
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    let status_cols =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(20)]).split(rows[0]);

    let status_line = if app.is_notifications_tab() {
        Line::from(Span::styled(
            "notifications",
            Style::default().fg(Color::Cyan),
        ))
    } else {
        let mut parts = vec!["filter: all".to_string()];
        if !app.author_filter.is_empty() {
            let author_label = if app.author_filter.len() == 1 {
                format!("@{}", app.author_filter.iter().next().unwrap())
            } else {
                format!("{} authors", app.author_filter.len())
            };
            parts[0] = format!("filter: {author_label}");
        }
        if !app.mr_query.is_empty() {
            parts.push(format!("title: \"{}\"", app.mr_query));
        }
        if app.new_notification_count > 0 {
            parts.push(format!(
                "{} new notification(s)",
                app.new_notification_count
            ));
        }
        let active = !app.author_filter.is_empty() || !app.mr_query.is_empty();
        Line::from(Span::styled(
            parts.join("  |  "),
            Style::default().fg(if active { Color::Green } else { Color::Gray }),
        ))
    };
    f.render_widget(Paragraph::new(status_line), status_cols[0]);

    let poll = if let Some(kind) = app.active_copied() {
        Paragraph::new(Line::from(Span::styled(
            kind.message(),
            Style::default().fg(Color::Black).bg(Color::Green),
        )))
    } else if app.fetching {
        Paragraph::new(Line::from(Span::styled(
            "Fetching",
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )))
    } else if app.showing_cached_mrs() {
        Paragraph::new(Line::from(Span::styled(
            "offline",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )))
    } else if let Some(err) = &app.poll_error {
        Paragraph::new(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::White).bg(Color::Red),
        )))
    } else {
        let secs = app.seconds_to_next_poll();
        Paragraph::new(Line::from(Span::styled(
            format!("next poll in {secs:>2}s"),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )))
    }
    .alignment(Alignment::Right);
    f.render_widget(poll, status_cols[1]);

    let keys_hint = if app.is_notifications_tab() {
        " [Tab/←→] tab  [0-9/n] jump  [↑↓] select  [Enter] open  [c] copy  [r] refresh  [?] help  [q] quit"
    } else {
        " [Tab/←→] tab  [0-9/n] jump  [↑↓] select  [/] filter  [a] authors  [f] clear  [Enter] open  [r] refresh  [?] help  [q] quit"
    };
    let keys = Paragraph::new(Line::from(Span::styled(
        keys_hint,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(keys, rows[1]);
}

fn render_help(f: &mut Frame, app: &App) {
    let area = centered_rect(f.area(), 50, 50);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Hotkeys ")
        .title_bottom(" ? / Esc close ")
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(Span::styled(
            "More hotkeys",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  H / L        reorder repo tab left / right"),
        Line::from("  m            toggle mine-only author filter"),
        Line::from("  a            select author filter"),
        Line::from("  f            clear all filters"),
        Line::from("  /            filter by title or IID"),
        Line::from("  n            jump to notifications tab"),
        Line::from("  0-9          jump to repo tab (0/1=first, 9=last)"),
        Line::from("  c            copy URL to clipboard"),
        Line::from("  C            copy MR reference (e.g. !2191)"),
        Line::from("  Ctrl-Shift-C copy menu (URL, ID, link, branch)"),
        Line::from("  s            open repo selector"),
    ];
    if app.cmux_available {
        lines.push(Line::from("  Alt-Enter  open in cmux split"));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_copy_menu(f: &mut Frame, app: &App) {
    let Some(cursor) = app.copy_menu else {
        return;
    };

    let area = centered_fixed(f.area(), 52, 6);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Copy ")
        .title_bottom(" ↑↓ select  Enter copy  Esc cancel ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let items: Vec<ListItem> = CopyChoice::ALL
        .iter()
        .map(|choice| ListItem::new(Line::from(choice.label())))
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(cursor));
    f.render_stateful_widget(list, inner, &mut state);
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let vertical = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center);
    let horizontal = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center);
    let [area] = vertical.areas(area);
    let [area] = horizontal.areas(area);
    area
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([Constraint::Percentage(percent_y)]).flex(Flex::Center);
    let horizontal = Layout::horizontal([Constraint::Percentage(percent_x)]).flex(Flex::Center);
    let [area] = vertical.areas(area);
    let [area] = horizontal.areas(area);
    area
}

fn render_selector(f: &mut Frame, app: &App) {
    let Some(sel) = app.selector.as_ref() else {
        return;
    };

    let area = centered_rect(f.area(), 70, 80);
    f.render_widget(Clear, area);

    let title = format!(
        " Select repos  |  filter: {}  ({} selected, {} shown) ",
        sel.filter.label(),
        sel.selected.len(),
        sel.visible().len(),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(" Space toggle  Tab filter  / search  Enter save  Esc cancel ")
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if sel.loading {
        f.render_widget(Paragraph::new("Loading projects...").dim(), inner);
        return;
    }
    if let Some(err) = &sel.error {
        f.render_widget(Paragraph::new(format!("Error: {err}")).red(), inner);
        return;
    }
    if sel.projects.is_empty() {
        f.render_widget(Paragraph::new("No projects for this filter.").dim(), inner);
        return;
    }

    // Split: search box on top, list below.
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    render_search_box(f, sel, parts[0]);
    let list_area = parts[1];

    if sel.visible().is_empty() {
        f.render_widget(
            Paragraph::new(format!("No matches for \"{}\".", sel.query)).dim(),
            list_area,
        );
        return;
    }

    let mut cursor_idx = 0;
    let items: Vec<ListItem> = sel
        .rows()
        .into_iter()
        .enumerate()
        .map(|(i, row)| match row {
            DisplayRow::Group(name) => ListItem::new(Line::from(Span::styled(
                name.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))),
            DisplayRow::Project {
                project,
                selected,
                is_cursor,
            } => {
                if is_cursor {
                    cursor_idx = i;
                }
                let checkbox = if selected { "[x]" } else { "[ ]" };
                let style = if selected {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(
                    format!("  {checkbox}  {}", project.name),
                    style,
                )))
            }
        })
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(cursor_idx));
    f.render_stateful_widget(list, list_area, &mut state);
}

fn render_search_box(f: &mut Frame, sel: &crate::selector::RepoSelector, area: Rect) {
    let line = if sel.search_active {
        Line::from(vec![
            Span::styled("/ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                sel.query.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ])
    } else if !sel.query.is_empty() {
        Line::from(Span::styled(
            format!("/ {}", sel.query),
            Style::default().fg(Color::Gray),
        ))
    } else {
        Line::from(Span::styled(
            "/ to search by name",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn render_author_selector(f: &mut Frame, app: &App) {
    let Some(sel) = app.author_selector.as_ref() else {
        return;
    };

    let area = centered_rect(f.area(), 50, 60);
    f.render_widget(Clear, area);

    let title = format!(
        " Filter by author  ({} selected, {} shown) ",
        sel.selected.len(),
        sel.visible().len(),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(" Space toggle  / filter  Enter apply  Esc cancel ")
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if sel.authors.is_empty() {
        f.render_widget(Paragraph::new("No authors in this repo yet.").dim(), inner);
        return;
    }

    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    render_author_search_box(f, sel, parts[0]);
    let list_area = parts[1];

    if sel.visible().is_empty() {
        f.render_widget(
            Paragraph::new(format!("No matches for \"{}\".", sel.query)).dim(),
            list_area,
        );
        return;
    }

    let mut cursor_idx = 0;
    let items: Vec<ListItem> = sel
        .visible()
        .into_iter()
        .enumerate()
        .map(|(i, author)| {
            if i == sel.cursor {
                cursor_idx = i;
            }
            let checkbox = if sel.selected.contains(&author.username) {
                "[x]"
            } else {
                "[ ]"
            };
            let label = if author.name.is_empty() {
                format!("@{username}", username = author.username)
            } else {
                format!(
                    "{name} (@{username})",
                    name = author.name,
                    username = author.username
                )
            };
            let style = if sel.selected.contains(&author.username) {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("  {checkbox}  {label}"),
                style,
            )))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(cursor_idx));
    f.render_stateful_widget(list, list_area, &mut state);
}

fn render_author_search_box(f: &mut Frame, sel: &crate::app::AuthorSelector, area: Rect) {
    let line = if sel.search_active {
        Line::from(vec![
            Span::styled("/ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                sel.query.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ])
    } else if !sel.query.is_empty() {
        Line::from(Span::styled(
            format!("/ {}", sel.query),
            Style::default().fg(Color::Gray),
        ))
    } else {
        Line::from(Span::styled(
            "type to filter authors",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::CopiedKind;
    use crate::config::RepoCfg;
    use crate::gitlab::{CiStatus, MergeRequest};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample_mr() -> MergeRequest {
        MergeRequest {
            iid: "12".into(),
            title: "Add copy menu".into(),
            web_url: "https://git.example/group/repo/-/merge_requests/12".into(),
            author_name: String::new(),
            author_username: "alice".into(),
            approved_by: Vec::new(),
            approved: false,
            draft: false,
            has_conflicts: false,
            notes_count: 0,
            last_note_author: None,
            last_note_at: None,
            ci: CiStatus::None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            source_branch: "feature-x".into(),
            last_update: None,
        }
    }

    #[test]
    fn copy_menu_and_confirmation_render() {
        let mut app = App::new(
            "git.example".into(),
            vec![RepoCfg {
                name: Some("repo".into()),
                path: "group/repo".into(),
            }],
            String::new(),
            false,
            false,
        );
        app.repo_states[0].mrs = vec![sample_mr()];
        app.repo_states[0].loaded = true;
        app.table_state.select(Some(0));
        app.open_copy_menu();
        app.show_copied(CopiedKind::Link);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let view = format!("{}", terminal.backend());

        assert!(view.contains("MR URL"), "{view}");
        assert!(view.contains("MR ID"), "{view}");
        assert!(view.contains("  Link "), "{view}");
        assert!(!view.contains("pasteable"), "{view}");
        assert!(view.contains("Branch name"), "{view}");
        assert!(view.contains("Link copied!"), "{view}");
    }
}
