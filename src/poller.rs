use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::app::{FetchResult, NotificationUpdate, PollEvent, PollUpdate, POLL_INTERVAL};
use crate::config::RepoCfg;
use crate::gitlab;
use crate::notifications::NotificationStore;

/// Spawn the background poll task.
///
/// Fetches all repos immediately, then every `POLL_INTERVAL`, or whenever a
/// value is received on `refresh_rx` (e.g. the `r` hotkey). A `(host, repos)`
/// pair on `reconfigure_rx` swaps the target host + repos and triggers an
/// immediate poll (used after first-run setup and after saving the selector).
/// Notification polling runs in the background so MR fetches are not blocked.
/// Results are sent on the returned receiver.
pub fn spawn(
    initial_host: String,
    initial_repos: Vec<RepoCfg>,
    initial_user: String,
    initial_notifications: NotificationStore,
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
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        // The first tick fires immediately, giving us an initial fetch.
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                maybe = refresh_rx.recv() => {
                    // `None` means the app dropped the sender; shut down.
                    if maybe.is_none() { break; }
                    interval.reset();
                }
                maybe = reconfigure_rx.recv() => {
                    match maybe {
                        Some((new_host, new_repos)) => {
                            host = new_host;
                            repos = new_repos;
                            interval.reset();
                        }
                        None => break,
                    }
                }
                maybe = user_rx.recv() => {
                    if let Some(user) = maybe {
                        current_user = user;
                    }
                }
            }

            if tx.send(PollEvent::Started).await.is_err() {
                break;
            }
            let update = poll_mrs(&host, &repos).await;
            if tx.send(PollEvent::Finished(update)).await.is_err() {
                break; // app gone
            }

            // Skip starting a new notification poll while the previous one runs.
            if notif_task.as_ref().is_some_and(|t| !t.is_finished()) {
                continue;
            }

            let store = Arc::clone(&notification_store);
            let tx = tx.clone();
            let host = host.clone();
            let repos = repos.clone();
            let user = current_user.clone();
            notif_task = Some(tokio::spawn(async move {
                let mut store = store.lock().await;
                match crate::notifications::poll(&host, &repos, &user, &mut store).await {
                    Ok(count) => {
                        let updated = store.clone();
                        drop(store);
                        let _ = tx
                            .send(PollEvent::NotificationsUpdated(NotificationUpdate {
                                store: updated,
                                new_count: count,
                            }))
                            .await;
                    }
                    Err(e) => eprintln!("warning: notification poll failed: {e}"),
                }
            }));
        }
    });

    rx
}

async fn poll_mrs(host: &str, repos: &[RepoCfg]) -> PollUpdate {
    let futures = repos.iter().map(|repo| async move {
        let result = gitlab::fetch_merge_requests(host, &repo.path)
            .await
            .map_err(|e| e.to_string()) as FetchResult;
        (repo.path.clone(), result)
    });
    let results = futures::future::join_all(futures).await;
    PollUpdate { results }
}
