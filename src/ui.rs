use chrono::Utc;
use chrono_humanize::HumanTime;
use ratatui::{
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Padding, Paragraph, Row, Table, Tabs},
    Frame,
};

use crate::app::App;
use crate::gitlab::{CiStatus, MergeRequest};
use crate::selector::DisplayRow;

pub fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(3), // tabs
        Constraint::Min(1),    // table
        Constraint::Length(2), // footer
    ])
    .split(f.area());

    render_tabs(f, app, chunks[0]);
    render_table(f, app, chunks[1]);
    render_footer(f, app, chunks[2]);

    if app.awaiting_url {
        render_url_prompt(f, app);
    } else if app.selector.is_some() {
        render_selector(f, app);
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
        let msg = if app.mine_only {
            "No merge requests authored by you here."
        } else {
            "No open merge requests."
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
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1);

    let rows: Vec<Row> = mrs.iter().map(|mr| build_row(mr)).collect();
    drop(mrs); // Row owns its content; release the immutable borrow on `app`.

    let widths = [
        Constraint::Percentage(50),
        Constraint::Percentage(25),
        Constraint::Length(10),
        Constraint::Length(12),
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

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn build_row(mr: &MergeRequest) -> Row<'static> {
    // Title cell: title + author underneath.
    let author = if mr.author_name.is_empty() {
        format!("@{}", mr.author_username)
    } else {
        format!("{} (@{})", mr.author_name, mr.author_username)
    };
    let title_cell = Cell::from(Text::from(vec![
        Line::from(Span::styled(
            format!("!{} {}", mr.iid, mr.title),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(author, Style::default().fg(Color::Gray))),
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
        Line::from(Span::styled(second, Style::default().fg(Color::Gray))),
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
        CiStatus::Pass => ("pass".to_string(), Color::Green),
        CiStatus::Fail => ("fail".to_string(), Color::Red),
        CiStatus::InProgress => ("in progress".to_string(), Color::Yellow),
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

    Row::new(vec![title_cell, comments_cell, status_cell, ci_cell, age_cell]).height(2)
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let secs = app.seconds_to_next_poll();
    let mine = if app.mine_only { "mine" } else { "all" };

    let status_line = Line::from(vec![
        Span::styled(
            format!(" next poll in {secs:>2}s "),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled(
            format!("filter: {mine}"),
            Style::default().fg(if app.mine_only {
                Color::Green
            } else {
                Color::Gray
            }),
        ),
    ]);

    let keys = Line::from(Span::styled(
        " [Tab/←→] repo  [↑↓] select  [m] mine  [Enter] open  [c] copy url  [C] copy id  [s] repos  [r] refresh  [q] quit",
        Style::default().fg(Color::DarkGray),
    ));

    let p = Paragraph::new(Text::from(vec![status_line, keys]));
    f.render_widget(p, area);
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
