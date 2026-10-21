use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;

use crate::app::{FetchResult, NotificationUpdate, PollEvent, PollUpdate};
use crate::config::RepoCfg;
use crate::gitlab::{self, MergeRequest};
use crate::notifications::NotificationStore;

/// How often the tab on screen is refreshed while the terminal is focused.
pub const ACTIVE_POLL: Duration = Duration::from_secs(15);
/// How often a tab that is not on screen is refreshed while focused.
pub const BACKGROUND_POLL: Duration = Duration::from_secs(60);
/// How often everything is refreshed while the terminal is unfocused.
pub const UNFOCUSED_POLL: Duration = Duration::from_secs(5 * 60);

/// Which tab the user is looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchView {
    Repo(String),
    Notifications,
}

/// The visible tab and whether the terminal window has focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    pub view: WatchView,
    pub focused: bool,
}

/// Interval for a target that is or is not the tab on screen.
pub fn poll_after(active: bool, focused: bool) -> Duration {
    if !focused {
        UNFOCUSED_POLL
    } else if active {
        ACTIVE_POLL
    } else {
        BACKGROUND_POLL
    }
}

/// `last` is missing on startup, which is always due.
pub fn fetch_due(last: Option<Instant>, now: Instant, active: bool, focused: bool) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t) >= poll_after(active, focused))
}

/// Opening a tab refreshes it when its data is older than the focused 15s
/// interval, including while the window is unfocused.
pub fn fetch_due_on_switch(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t) >= ACTIVE_POLL)
}

fn next_at(last: Option<Instant>, now: Instant, active: bool, focused: bool) -> Instant {
    match last {
        Some(t) => t + poll_after(active, focused),
        None => now,
    }
}

/// Spawn the background poll task.
///
/// Each repo and the notification feed have their own deadline. The tab on
/// screen refreshes every 15 seconds, the others every minute, and all of them
/// every five minutes while the terminal is unfocused. `r` refreshes everything
/// immediately. A new repo list is fetched immediately.
pub fn spawn(
    initial_host: String,
    initial_repos: Vec<RepoCfg>,
    initial_user: String,
    initial_notifications: NotificationStore,
    mut watch_rx: watch::Receiver<Watch>,
    mut refresh_rx: mpsc::Receiver<()>,
    mut reconfigure_rx: mpsc::Receiver<(String, Vec<RepoCfg>)>,
    mut user_rx: mpsc::Receiver<String>,
) -> mpsc::Receiver<PollEvent> {
    let (tx, rx) = mpsc::channel::<PollEvent>(8);

    tokio::spawn(async move {
        let mut host = initial_host;
        let mut repos = initial_repos;
        let mut current_user = initial_user;
        let notification_store = Arc::new(Mutex::new(initial_notifications));
        let mut notif_task: Option<JoinHandle<()>> = None;
        let mut last_repo: HashMap<String, Instant> = HashMap::new();
        let mut previous_mrs: HashMap<String, Vec<MergeRequest>> = HashMap::new();
        let mut last_notifications: Option<Instant> = None;
        let mut notif_again = false;
        let mut seen_view = watch_rx.borrow().view.clone();
        let mut announced: Option<Instant> = None;
        let mut startup = true;

        loop {
            let mut force_repos = false;
            let mut force_all = false;
            if !startup {
                tokio::select! {
                    maybe = refresh_rx.recv() => {
                        if maybe.is_none() { break; }
                        force_all = true;
                    }
                    maybe = reconfigure_rx.recv() => {
                        match maybe {
                            Some((new_host, new_repos)) => {
                                host = new_host;
                                repos = new_repos;
                                last_repo.retain(|path, _| repos.iter().any(|r| &r.path == path));
                                previous_mrs.retain(|path, _| repos.iter().any(|r| &r.path == path));
                                force_repos = true;
                            }
                            None => break,
                        }
                    }
                    maybe = user_rx.recv() => {
                        if let Some(user) = maybe {
                            current_user = user;
                        } else {
                            break;
                        }
                    }
                    result = watch_rx.changed() => {
                        if result.is_err() { break; }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
            startup = false;

            while refresh_rx.try_recv().is_ok() {
                force_all = true;
            }

            let watch = watch_rx.borrow().clone();
            let switched = watch.view != seen_view;
            seen_view = watch.view.clone();
            let now = Instant::now();

            let due: Vec<RepoCfg> = repos
                .iter()
                .filter(|repo| {
                    let last = last_repo.get(&repo.path).copied();
                    let active = matches!(&watch.view, WatchView::Repo(path) if path == &repo.path);
                    force_all
                        || force_repos
                        || (switched && active && fetch_due_on_switch(last, now))
                        || fetch_due(last, now, active, watch.focused)
                })
                .cloned()
                .collect();

            if !due.is_empty() {
                let visible = matches!(&watch.view, WatchView::Repo(path) if due.iter().any(|r| &r.path == path));
                if tx.send(PollEvent::Started { visible }).await.is_err() {
                    break;
                }
                let started = Instant::now();
                let update = poll_mrs(&host, &due, &previous_mrs).await;
                log::info!(
                    "mr poll: fetched {} of {} repos in {:.1}s ({} failed)",
                    due.len(),
                    repos.len(),
                    started.elapsed().as_secs_f64(),
                    update.results.iter().filter(|(_, r)| r.is_err()).count()
                );
                let fetched_at = Instant::now();
                for (path, result) in &update.results {
                    last_repo.insert(path.clone(), fetched_at);
                    if let Ok(mrs) = result {
                        previous_mrs.insert(path.clone(), mrs.clone());
                    }
                }
                if tx.send(PollEvent::Finished(update)).await.is_err() {
                    break;
                }
            }

            let notif_active = watch.view == WatchView::Notifications;
            let notif_due = force_all
                || notif_again
                || (switched && notif_active && fetch_due_on_switch(last_notifications, now))
                || fetch_due(last_notifications, now, notif_active, watch.focused);
            if notif_due && notif_task.as_ref().is_none_or(|t| t.is_finished()) {
                notif_again = false;
                last_notifications = Some(Instant::now());
                let store = Arc::clone(&notification_store);
                let tx = tx.clone();
                let host = host.clone();
                let user = current_user.clone();
                notif_task = Some(tokio::spawn(async move {
                    let mut store = store.lock().await;
                    match crate::notifications::poll(&host, &user, &mut store).await {
                        Ok(new_items) => {
                            let new_count = new_items.len();
                            let updated = store.clone();
                            drop(store);
                            let _ = tx
                                .send(PollEvent::NotificationsUpdated(NotificationUpdate {
                                    store: updated,
                                    new_count,
                                    new_items,
                                }))
                                .await;
                        }
                        Err(_e) => {}
                    }
                }));
            } else if notif_due {
                notif_again = true;
            }

            let active_last = match &watch.view {
                WatchView::Repo(path) => last_repo.get(path).copied(),
                WatchView::Notifications => last_notifications,
            };
            let next = next_at(active_last, Instant::now(), true, watch.focused);
            if announced != Some(next) {
                announced = Some(next);
                if tx.send(PollEvent::NextPoll(next)).await.is_err() {
                    break;
                }
            }
        }
    });

    rx
}

async fn poll_mrs(
    host: &str,
    repos: &[RepoCfg],
    previous: &HashMap<String, Vec<MergeRequest>>,
) -> PollUpdate {
    let futures = repos.iter().map(|repo| {
        let previous = previous.get(&repo.path).cloned().unwrap_or_default();
        async move {
            let result = gitlab::fetch_merge_requests(host, &repo.path, &previous)
                .await
                .map_err(|e| e.to_string()) as FetchResult;
            (repo.path.clone(), result)
        }
    });
    let results = futures::future::join_all(futures).await;
    PollUpdate { results }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn before(now: Instant, secs: u64) -> Instant {
        now - Duration::from_secs(secs)
    }

    #[test]
    fn focused_open_tab_is_due_after_15s_and_others_after_60s() {
        let now = Instant::now();
        assert!(!fetch_due(Some(before(now, 10)), now, true, true));
        assert!(fetch_due(Some(before(now, 15)), now, true, true));
        assert!(!fetch_due(Some(before(now, 30)), now, false, true));
        assert!(fetch_due(Some(before(now, 60)), now, false, true));
        assert!(fetch_due(None, now, false, true));
    }

    #[test]
    fn notifications_use_the_open_tab_interval_only_while_selected() {
        let now = Instant::now();
        assert!(fetch_due(Some(before(now, 15)), now, true, true));
        assert!(!fetch_due(Some(before(now, 15)), now, false, true));
        assert!(fetch_due(Some(before(now, 60)), now, false, true));
    }

    #[test]
    fn unfocused_window_waits_five_minutes() {
        let now = Instant::now();
        assert!(!fetch_due(Some(before(now, 60)), now, true, false));
        assert!(fetch_due(Some(before(now, 300)), now, true, false));
        assert!(fetch_due(Some(before(now, 300)), now, false, false));
    }

    #[test]
    fn switching_tabs_is_due_only_after_15s() {
        let now = Instant::now();
        assert!(!fetch_due_on_switch(Some(before(now, 5)), now));
        assert!(fetch_due_on_switch(Some(before(now, 15)), now));
        assert!(fetch_due_on_switch(None, now));
    }
}
