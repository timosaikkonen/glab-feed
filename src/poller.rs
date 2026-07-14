use tokio::sync::mpsc;

use crate::app::{FetchResult, PollEvent, PollUpdate, POLL_INTERVAL};
use crate::config::RepoCfg;
use crate::gitlab;

/// Spawn the background poll task.
///
/// Fetches all repos immediately, then every `POLL_INTERVAL`, or whenever a
/// value is received on `refresh_rx` (e.g. the `r` hotkey). A `(host, repos)`
/// pair on `reconfigure_rx` swaps the target host + repos and triggers an
/// immediate poll (used after first-run setup and after saving the selector).
/// Results are sent on the returned receiver.
pub fn spawn(
    initial_host: String,
    initial_repos: Vec<RepoCfg>,
    mut refresh_rx: mpsc::Receiver<()>,
    mut reconfigure_rx: mpsc::Receiver<(String, Vec<RepoCfg>)>,
) -> mpsc::Receiver<PollEvent> {
    let (tx, rx) = mpsc::channel::<PollEvent>(8);

    tokio::spawn(async move {
        let mut host = initial_host;
        let mut repos = initial_repos;
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
            }

            if tx.send(PollEvent::Started).await.is_err() {
                break;
            }
            let update = poll_all(&host, &repos).await;
            if tx.send(PollEvent::Finished(update)).await.is_err() {
                break; // app gone
            }
        }
    });

    rx
}

async fn poll_all(host: &str, repos: &[RepoCfg]) -> PollUpdate {
    let futures = repos.iter().map(|repo| async move {
        let result = gitlab::fetch_merge_requests(host, &repo.path)
            .await
            .map_err(|e| e.to_string()) as FetchResult;
        (repo.path.clone(), result)
    });
    let results = futures::future::join_all(futures).await;
    PollUpdate { results }
}
