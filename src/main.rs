mod app;
mod config;
mod gitlab;
mod notifications;
mod poller;
mod selector;
mod ui;

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;

use app::{App, ApprovalEvent, CopiedKind, PollEvent, SelectorUpdate};
use config::RepoCfg;
use notifications::{Notification, NotificationStore};
use selector::RepoFilter;

/// Shared channels the event loop needs when handling keys.
struct Channels {
    refresh_tx: mpsc::Sender<()>,
    reconfigure_tx: mpsc::Sender<(String, Vec<RepoCfg>)>,
    selector_res_tx: mpsc::Sender<SelectorUpdate>,
    user_res_tx: mpsc::Sender<String>,
    user_poll_tx: mpsc::Sender<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Missing config -> first-run setup inside the TUI. A present-but-invalid
    // config is a hard error.
    let cfg = if config::config_path().exists() {
        match config::load() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let host = cfg.as_ref().map(|c| c.host.clone()).unwrap_or_default();
    let repos = cfg.as_ref().map(|c| c.repos.clone()).unwrap_or_default();

    let current_user = if host.is_empty() {
        String::new()
    } else {
        gitlab::fetch_current_user(&host).await.unwrap_or_else(|e| {
            eprintln!("warning: could not determine current user: {e}");
            String::new()
        })
    };

    let mut app = App::new(
        host.clone(),
        repos.clone(),
        current_user.clone(),
        cmux_available(),
        cmux_notify_enabled(),
    );
    if cfg.is_none() {
        app.start_setup();
    }

    let (refresh_tx, refresh_rx) = mpsc::channel::<()>(1);
    let (reconfigure_tx, reconfigure_rx) = mpsc::channel::<(String, Vec<RepoCfg>)>(1);
    let (selector_res_tx, mut selector_res_rx) = mpsc::channel::<SelectorUpdate>(4);
    let (user_res_tx, mut user_res_rx) = mpsc::channel::<String>(1);
    let (user_poll_tx, user_poll_rx) = mpsc::channel::<String>(1);
    let notification_store = NotificationStore::load();
    let mut updates = poller::spawn(
        host,
        repos,
        current_user,
        notification_store,
        refresh_rx,
        reconfigure_rx,
        user_poll_rx,
    );

    let channels = Channels {
        refresh_tx,
        reconfigure_tx,
        selector_res_tx,
        user_res_tx,
        user_poll_tx,
    };

    let mut terminal = ratatui::init();
    let result = run(
        &mut terminal,
        &mut app,
        &mut updates,
        &mut selector_res_rx,
        &mut user_res_rx,
        &channels,
    )
    .await;
    ratatui::restore();

    if let Some(err) = app.fatal_error {
        eprintln!("{err}");
        std::process::exit(1);
    }
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    updates: &mut mpsc::Receiver<PollEvent>,
    selector_res_rx: &mut mpsc::Receiver<SelectorUpdate>,
    user_res_rx: &mut mpsc::Receiver<String>,
    channels: &Channels,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    terminal.draw(|f| ui::render(f, app))?;

    loop {
        tokio::select! {
            // Redraw once a second so the countdown stays live.
            _ = tick.tick() => {
                app.clear_expired_copied();
            }

            // Hide the copy confirmation promptly when its timer expires.
            _ = async {
                let until = app.copied.expect("guarded by if").1;
                tokio::time::sleep_until(tokio::time::Instant::from_std(until)).await;
            }, if app.copied.is_some() => {
                app.clear_expired_copied();
            }

            // Poll lifecycle from the poller.
            Some(event) = updates.recv() => {
                match event {
                    PollEvent::Started => app.handle_poll_started(),
                    PollEvent::Finished(update) => {
                        let approvals = app.handle_poll_finished(update);
                        if app.cmux_notify_enabled {
                            for a in &approvals {
                                cmux_notify_for_approval(a);
                            }
                        }
                    }
                    PollEvent::NotificationsUpdated(update) => {
                        if app.cmux_notify_enabled {
                            for n in &update.new_items {
                                cmux_notify_for_notification(n);
                            }
                        }
                        app.handle_notifications_updated(update);
                    }
                }
            }

            // Projects list for the repo selector.
            Some(update) = selector_res_rx.recv() => {
                app.apply_selector_update(update);
            }

            // Current user resolved after first-run URL entry.
            Some(user) = user_res_rx.recv() => {
                app.set_current_user(user.clone());
                let _ = channels.user_poll_tx.try_send(user);
            }

            // Terminal input.
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(app, key, channels);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }

        if app.should_quit {
            break;
        }
        terminal.draw(|f| ui::render(f, app))?;
    }

    Ok(())
}

fn copy_to_clipboard(text: &str) -> bool {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        cb.set_text(text.to_string()).is_ok()
    } else {
        false
    }
}

fn cmux_available() -> bool {
    std::process::Command::new("sh")
        .args(["-c", "command -v cmux >/dev/null 2>&1"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn cmux_notify_enabled() -> bool {
    cmux_available() && std::env::var("CMUX_WORKSPACE_ID").is_ok()
}

fn cmux_notify(title: &str, subtitle: &str, body: &str) {
    let _ = std::process::Command::new("cmux")
        .args(["notify", "--title", title, "--subtitle", subtitle, "--body", body])
        .spawn();
}

fn notification_notify_parts(n: &Notification) -> (&'static str, String, String) {
    let subtitle = format!("@{} {}", n.author, n.kind.verb());
    let mr_ref = n.mr_iid.as_ref().map(|i| format!("!{i}")).unwrap_or_default();
    let body = if mr_ref.is_empty() {
        format!("{}: {}", n.repo, n.summary)
    } else {
        format!("{} {}: {}", n.repo, mr_ref, n.summary)
    };
    ("glab-feed", subtitle, body)
}

fn approval_notify_parts(a: &ApprovalEvent) -> (&'static str, String, String) {
    let subtitle = format!("@{} approved", a.approver);
    let body = format!("{} !{}: {}", a.repo, a.mr_iid, a.title);
    ("glab-feed", subtitle, body)
}

fn cmux_notify_for_notification(n: &Notification) {
    let (title, subtitle, body) = notification_notify_parts(n);
    cmux_notify(title, &subtitle, &body);
}

fn cmux_notify_for_approval(a: &ApprovalEvent) {
    let (title, subtitle, body) = approval_notify_parts(a);
    cmux_notify(title, &subtitle, &body);
}

#[derive(Deserialize)]
struct CmuxBrowserOpenResponse {
    surface_ref: String,
}

#[derive(Deserialize)]
struct CmuxListPanelsResponse {
    surfaces: Vec<CmuxPanelSurface>,
}

#[derive(Deserialize)]
struct CmuxPanelSurface {
    #[serde(rename = "ref")]
    surface_ref: String,
    #[serde(rename = "type")]
    surface_type: String,
}

fn cmux_browser_surface_exists(surface_ref: &str) -> bool {
    let Ok(output) = std::process::Command::new("cmux")
        .args(["--json", "list-panels"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(panels) = serde_json::from_slice::<CmuxListPanelsResponse>(&output.stdout) else {
        return false;
    };
    panels
        .surfaces
        .iter()
        .any(|s| s.surface_ref == surface_ref && s.surface_type == "browser")
}

fn open_in_cmux(app: &mut App, url: &str) {
    if let Some(stored) = app.cmux_surface_ref.clone() {
        if cmux_browser_surface_exists(&stored) {
            let _ = std::process::Command::new("cmux")
                .args(["--json", "browser", &stored, "open", url])
                .spawn();
            return;
        }
        app.cmux_surface_ref = None;
    }

    let Ok(output) = std::process::Command::new("cmux")
        .args(["--json", "browser", "open", url])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    if let Ok(resp) = serde_json::from_slice::<CmuxBrowserOpenResponse>(&output.stdout) {
        app.cmux_surface_ref = Some(resp.surface_ref);
    }
}

/// Spawn a background task fetching projects for `filter`.
fn spawn_project_fetch(host: String, filter: RepoFilter, tx: mpsc::Sender<SelectorUpdate>) {
    tokio::spawn(async move {
        let result = selector::fetch_projects(&host, filter)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(SelectorUpdate { filter, result }).await;
    });
}

/// Spawn a background task resolving the current user for `host`.
fn spawn_user_fetch(host: String, tx: mpsc::Sender<String>) {
    tokio::spawn(async move {
        if let Ok(user) = gitlab::fetch_current_user(&host).await {
            let _ = tx.send(user).await;
        }
    });
}

fn handle_key(app: &mut App, key: KeyEvent, channels: &Channels) {
    if app.awaiting_url {
        handle_url_key(app, key, channels);
        return;
    }
    if app.selector.is_some() {
        handle_selector_key(app, key, channels);
        return;
    }
    if app.author_selector.is_some() {
        handle_author_selector_key(app, key);
        return;
    }
    if app.mr_search_active {
        handle_mr_search_key(app, key);
        return;
    }
    if app.show_help {
        handle_help_key(app, key);
        return;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => app.should_quit = true,
        (KeyCode::Tab, _) | (KeyCode::Right, _) => app.next_tab(),
        (KeyCode::BackTab, _) | (KeyCode::Left, _) => app.prev_tab(),
        (KeyCode::Char('n'), _) => app.select_notifications_tab(),
        (KeyCode::Char(c @ '0'..='9'), _) => app.handle_digit_tab(c),
        (KeyCode::Char('r'), _) => {
            let _ = channels.refresh_tx.try_send(());
        }
        (KeyCode::Char('?'), _) => app.show_help = true,
        _ if app.is_notifications_tab() => match (key.code, key.modifiers) {
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.select_notification_next(),
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => app.select_notification_prev(),
            (KeyCode::Enter, KeyModifiers::ALT) if app.cmux_available => {
                if let Some(url) = app.selected_notification_url() {
                    open_in_cmux(app, &url);
                }
            }
            (KeyCode::Enter, _) => {
                if let Some(url) = app.selected_notification_url() {
                    let _ = open::that_detached(url);
                }
            }
            (KeyCode::Char('c'), _) => {
                if let Some(url) = app.selected_notification_url() {
                    if copy_to_clipboard(&url) {
                        app.show_copied(CopiedKind::Url);
                    }
                }
            }
            _ => {}
        },
        _ => match (key.code, key.modifiers) {
            (KeyCode::Char('H'), _) => app.move_tab_left(),
            (KeyCode::Char('L'), _) => app.move_tab_right(),
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.select_next(),
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => app.select_prev(),
            (KeyCode::Char('m'), _) => app.toggle_mine(),
            (KeyCode::Char('a'), _) => app.open_author_selector(),
            (KeyCode::Char('f'), _) => app.clear_filters(),
            (KeyCode::Char('s'), _) => {
                app.open_selector();
                if let Some(sel) = app.selector.as_ref() {
                    spawn_project_fetch(
                        app.host.clone(),
                        sel.filter,
                        channels.selector_res_tx.clone(),
                    );
                }
            }
            (KeyCode::Enter, KeyModifiers::ALT) if app.cmux_available => {
                if let Some(url) = app.selected_url() {
                    open_in_cmux(app, &url);
                }
            }
            (KeyCode::Enter, _) => {
                if let Some(url) = app.selected_url() {
                    let _ = open::that_detached(url);
                }
            }
            (KeyCode::Char('c'), _) => {
                if let Some(url) = app.selected_url() {
                    if copy_to_clipboard(&url) {
                        app.show_copied(CopiedKind::Url);
                    }
                }
            }
            (KeyCode::Char('C'), _) => {
                if let Some(id) = app.selected_id() {
                    if copy_to_clipboard(&id) {
                        app.show_copied(CopiedKind::Ref);
                    }
                }
            }
            (KeyCode::Char('/'), _) => app.enter_mr_search(),
            _ => {}
        },
    }
}

fn handle_author_selector_key(app: &mut App, key: KeyEvent) {
    if app.author_selector.as_ref().is_some_and(|s| s.search_active) {
        if let Some(sel) = app.author_selector.as_mut() {
            match (key.code, key.modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
                (KeyCode::Enter, _) => sel.exit_search(false),
                (KeyCode::Esc, _) => sel.exit_search(true),
                (KeyCode::Backspace, _) => sel.backspace_query(),
                (KeyCode::Char(c), _) => sel.push_query_char(c),
                _ => {}
            }
        }
        return;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
        (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => app.close_author_selector(false),
        (KeyCode::Char('/'), _) => {
            if let Some(sel) = app.author_selector.as_mut() {
                sel.enter_search();
            }
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            if let Some(sel) = app.author_selector.as_mut() {
                sel.move_down();
            }
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
            if let Some(sel) = app.author_selector.as_mut() {
                sel.move_up();
            }
        }
        (KeyCode::Char(' '), _) => {
            if let Some(sel) = app.author_selector.as_mut() {
                sel.toggle();
            }
        }
        (KeyCode::Enter, _) => app.close_author_selector(true),
        _ => {
            if let KeyCode::Char(c) = key.code {
                if let Some(sel) = app.author_selector.as_mut() {
                    sel.search_active = true;
                    sel.push_query_char(c);
                }
            }
        }
    }
}

fn handle_help_key(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
        (KeyCode::Esc, _) | (KeyCode::Char('?'), _) => app.show_help = false,
        _ => {}
    }
}

fn handle_mr_search_key(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
        (KeyCode::Enter, _) => app.exit_mr_search(false),
        (KeyCode::Esc, _) => app.exit_mr_search(true),
        (KeyCode::Backspace, _) => app.backspace_mr_query(),
        (KeyCode::Char(c), _) => app.push_mr_query_char(c),
        _ => {}
    }
}

fn handle_url_key(app: &mut App, key: KeyEvent, channels: &Channels) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
        (KeyCode::Esc, _) => app.should_quit = true,
        (KeyCode::Backspace, _) => app.backspace_url(),
        (KeyCode::Char(c), _) => app.push_url_char(c),
        (KeyCode::Enter, _) => {
            if let Some(host) = app.submit_url() {
                // Resolve the user and open the selector to pick repos.
                spawn_user_fetch(host.clone(), channels.user_res_tx.clone());
                app.open_selector();
                if let Some(sel) = app.selector.as_ref() {
                    spawn_project_fetch(host, sel.filter, channels.selector_res_tx.clone());
                }
            }
        }
        _ => {}
    }
}

fn handle_selector_key(app: &mut App, key: KeyEvent, channels: &Channels) {
    // Search-input mode captures typing; only Ctrl-C still quits.
    if app.selector.as_ref().is_some_and(|s| s.search_active) {
        if let Some(sel) = app.selector.as_mut() {
            match (key.code, key.modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
                (KeyCode::Enter, _) => sel.exit_search(false),
                (KeyCode::Esc, _) => sel.exit_search(true),
                (KeyCode::Backspace, _) => sel.backspace_query(),
                (KeyCode::Char(c), _) => sel.push_query_char(c),
                _ => {}
            }
        }
        return;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
        (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => app.close_selector(),
        (KeyCode::Char('/'), _) => {
            if let Some(sel) = app.selector.as_mut() {
                sel.enter_search();
            }
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            if let Some(sel) = app.selector.as_mut() {
                sel.move_down();
            }
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
            if let Some(sel) = app.selector.as_mut() {
                sel.move_up();
            }
        }
        (KeyCode::Char(' '), _) => {
            if let Some(sel) = app.selector.as_mut() {
                sel.toggle();
            }
        }
        (KeyCode::Tab, _) => {
            if let Some(sel) = app.selector.as_mut() {
                sel.filter = sel.filter.next();
                sel.loading = true;
                sel.error = None;
                let filter = sel.filter;
                spawn_project_fetch(app.host.clone(), filter, channels.selector_res_tx.clone());
            }
        }
        (KeyCode::Enter, _) => {
            let cfgs = app
                .selector
                .as_ref()
                .map(|s| s.to_repo_cfgs())
                .unwrap_or_default();
            if cfgs.is_empty() {
                if let Some(sel) = app.selector.as_mut() {
                    sel.error = Some("Select at least one repo before saving.".to_string());
                }
                return;
            }
            let cfg = config::Config {
                host: app.host.clone(),
                repos: cfgs.clone(),
            };
            if let Err(e) = config::save(&cfg) {
                if let Some(sel) = app.selector.as_mut() {
                    sel.error = Some(format!("Save failed: {e}"));
                }
                return;
            }
            app.first_run = false;
            app.set_repos(cfgs.clone());
            let _ = channels.reconfigure_tx.try_send((app.host.clone(), cfgs));
            app.close_selector();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use notifications::NotificationKind;

    #[test]
    fn notification_notify_parts_formats_body() {
        let n = Notification {
            id: "note:1".to_string(),
            kind: NotificationKind::Comment,
            at: Utc::now(),
            author: "alice".to_string(),
            summary: "Looks good".to_string(),
            url: String::new(),
            repo: "group/repo".to_string(),
            mr_iid: Some("42".to_string()),
        };
        let (title, subtitle, body) = notification_notify_parts(&n);
        assert_eq!(title, "glab-feed");
        assert_eq!(subtitle, "@alice commented");
        assert_eq!(body, "group/repo !42: Looks good");
    }

    #[test]
    fn approval_notify_parts_formats_body() {
        let a = ApprovalEvent {
            approver: "bob".to_string(),
            repo: "group/repo".to_string(),
            mr_iid: "7".to_string(),
            title: "Add feature".to_string(),
        };
        let (title, subtitle, body) = approval_notify_parts(&a);
        assert_eq!(title, "glab-feed");
        assert_eq!(subtitle, "@bob approved");
        assert_eq!(body, "group/repo !7: Add feature");
    }
}
