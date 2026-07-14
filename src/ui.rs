use chrono::Utc;
use chrono_humanize::HumanTime;
use ratatui::{
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Padding, Paragraph, Row, Table, Tabs},
    Frame,
};

/// Secondary text on the second line of two-line MR rows (author, last comment).
const MR_SECONDARY: Color = Color::DarkGray;

// Nerd Font glyphs (Font Awesome set): check, times, refresh, build, note, push.
const NF_CHECK: &str = "\u{f00c}";
const NF_TIMES: &str = "\u{f00d}";
const NF_REFRESH: &str = "\u{f021}";
const NF_BUILD: &str = "\u{f085}";
const NF_NOTE: &str = "\u{f075}";
const NF_PUSH: &str = "\u{f126}";

fn update_activity_glyph(activity: UpdateActivity) -> &'static str {
    match activity {
        UpdateActivity::Build => NF_BUILD,
        UpdateActivity::Note => NF_NOTE,
        UpdateActivity::Push => NF_PUSH,
    }
}

use crate::app::App;
use crate::gitlab::{CiStatus, MergeRequest, UpdateActivity};
use crate::selector::DisplayRow;

pub fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(3), // tabs
        Constraint::Length(1), // MR filter
        Constraint::Min(1),    // table
        Constraint::Length(2), // footer
    ])
    .split(f.area());

    render_tabs(f, app, chunks[0]);
    render_mr_search(f, app, chunks[1]);
    render_table(f, app, chunks[2]);
    render_footer(f, app, chunks[3]);

    if app.awaiting_url {
        render_url_prompt(f, app);
    } else if app.selector.is_some() {
        render_selector(f, app);
    } else if app.show_help {
        render_help(f);
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
    let titles: Vec<Line> = app
        .repos
        .iter()
        .enumerate()
        .map(|(i, repo)| {
            let count = app
                .repo_states
                .get(i)
                .map(|s| s.mrs.len())
                .unwrap_or(0);
            let err = app
                .repo_states
                .get(i)
                .map(|s| s.error.is_some())
                .unwrap_or(false);
            let mark = if err { " !" } else { "" };
            Line::from(format!(" {} ({}){} ", repo.label(), count, mark))
        })
        .collect();

    let tabs = Tabs::new(titles)
        .select(app.selected_tab)
        .block(Block::default().borders(Borders::ALL).title(" Repos "))
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
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Merge Requests ")
        .padding(Padding::horizontal(1));

    // Surface errors / loading / empty states.
    if let Some(state) = app.current_state() {
        if let Some(err) = &state.error {
            let p = Paragraph::new(format!("Error fetching this repo:\n{err}"))
                .red()
                .block(block);
            f.render_widget(p, area);
            return;
        }
        if !state.loaded {
            let p = Paragraph::new("Loading...").dim().block(block);
            f.render_widget(p, area);
            return;
        }
    }

    let mrs = app.visible_mrs();
    if mrs.is_empty() {
        let msg = if !app.mr_query.is_empty() {
            format!("No merge requests matching \"{}\".", app.mr_query)
        } else if app.mine_only {
            "No merge requests authored by you here.".to_string()
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
    let status_cell = Cell::from(Text::from(vec![Line::from(Span::styled(
        status_text,
        Style::default().fg(status_color),
    ))]));

    // CI cell.
    let (ci_text, ci_color) = match &mr.ci {
        CiStatus::Pass => (NF_CHECK.to_string(), Color::Green),
        CiStatus::Fail => (NF_TIMES.to_string(), Color::Red),
        CiStatus::InProgress => (NF_REFRESH.to_string(), Color::Yellow),
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

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let mine = if app.mine_only { "mine" } else { "all" };

    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    let status_cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(20)]).split(rows[0]);

    let filter = Paragraph::new(Line::from(Span::styled(
        format!("filter: {mine}"),
        Style::default().fg(if app.mine_only {
            Color::Green
        } else {
            Color::Gray
        }),
    )));
    f.render_widget(filter, status_cols[0]);

    let poll = if app.fetching {
        Paragraph::new(Line::from(Span::styled(
            "Fetching",
            Style::default().fg(Color::Black).bg(Color::Cyan),
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

    let keys = Paragraph::new(Line::from(Span::styled(
        " [Tab/←→] repo  [↑↓] select  [/] filter  [Enter] open  [r] refresh  [?] help  [q] quit",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(keys, rows[1]);
}

fn render_help(f: &mut Frame) {
    let area = centered_rect(f.area(), 50, 50);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Hotkeys ")
        .title_bottom(" ? / Esc close ")
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines = Text::from(vec![
        Line::from(Span::styled("More hotkeys", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from("  H / L        reorder tab left / right"),
        Line::from("  m            toggle mine-only filter"),
        Line::from("  c            copy MR URL to clipboard"),
        Line::from("  C            copy MR reference (e.g. !2191)"),
        Line::from("  s            open repo selector"),
    ]);
    f.render_widget(Paragraph::new(lines), inner);
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
        f.render_widget(
            Paragraph::new("No projects for this filter.").dim(),
            inner,
        );
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
