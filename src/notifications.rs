use std::collections::HashSet;
use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{self, RepoCfg};
use crate::gitlab::{self, MrCandidate, RestTodo};

pub const MAX_ITEMS: usize = 300;
const MAX_BODY: usize = 200;

const TODO_ACTIONS: &[&str] = &[
    "review_requested",
    "review_submitted",
    "build_failed",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Comment,
    ReviewSubmitted,
    ReviewRequested,
    PipelineFailed,
}

impl NotificationKind {
    pub fn verb(self) -> &'static str {
        match self {
            NotificationKind::Comment => "commented",
            NotificationKind::ReviewSubmitted => "reviewed",
            NotificationKind::ReviewRequested => "requested review on",
            NotificationKind::PipelineFailed => "pipeline failed on",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub kind: NotificationKind,
    pub at: DateTime<Utc>,
    pub author: String,
    pub summary: String,
    pub url: String,
    pub repo: String,
    pub mr_iid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationStore {
    pub last_note_id: u64,
    pub last_todo_id: u64,
    pub seeded: bool,
    pub items: Vec<Notification>,
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self {
            last_note_id: 0,
            last_todo_id: 0,
            seeded: false,
            items: Vec::new(),
        }
    }
}

impl NotificationStore {
    pub fn load() -> Self {
        let path = config::notifications_path();
        if !path.exists() {
            return Self::default();
        }
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config::notifications_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating notifications dir {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serializing notifications")?;
        fs::write(&path, format!("{json}\n"))
            .with_context(|| format!("writing notifications at {}", path.display()))?;
        Ok(())
    }

    fn existing_ids(&self) -> HashSet<&str> {
        self.items.iter().map(|n| n.id.as_str()).collect()
    }

    /// Prepend new items, dedup by id, cap at MAX_ITEMS.
    pub fn prepend(&mut self, mut new: Vec<Notification>) {
        if new.is_empty() {
            return;
        }
        let existing = self.existing_ids();
        new.retain(|n| !existing.contains(n.id.as_str()));
        new.sort_by_key(|n| n.at);
        new.reverse();
        for item in new {
            self.items.insert(0, item);
        }
        self.items.truncate(MAX_ITEMS);
    }
}

pub fn snippet(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() > MAX_BODY {
        format!("{}...", &one_line[..MAX_BODY - 3])
    } else if one_line.is_empty() {
        "GitLab activity".to_string()
    } else {
        one_line
    }
}

fn todo_kind(action: &str) -> Option<NotificationKind> {
    if !TODO_ACTIONS.contains(&action) {
        return None;
    }
    match action {
        "review_submitted" => Some(NotificationKind::ReviewSubmitted),
        "review_requested" => Some(NotificationKind::ReviewRequested),
        "build_failed" => Some(NotificationKind::PipelineFailed),
        _ => None,
    }
}

fn mr_iid_from_url(url: &str) -> Option<String> {
    url.split("/-/merge_requests/")
        .nth(1)
        .and_then(|rest| rest.split(['#', '/']).next())
        .map(|s| s.to_string())
}

fn repo_from_todo_url(url: &str, configured: &[RepoCfg]) -> Option<String> {
    // https://host/group/repo/-/merge_requests/42
    let after_host = url.split("://").nth(1)?;
    let path_part = after_host.split('/').skip(1).collect::<Vec<_>>().join("/");
    let mr_marker = "/-/merge_requests/";
    let repo_end = path_part.find(mr_marker)?;
    let repo = path_part[..repo_end].to_string();
    if configured.iter().any(|r| r.path == repo) {
        Some(repo)
    } else {
        None
    }
}

async fn collect_new_notes(
    host: &str,
    candidate: &MrCandidate,
    last_note_id: u64,
    current_user: &str,
) -> Result<Vec<Notification>> {
    let notes = gitlab::fetch_mr_notes(host, candidate.project_id, candidate.iid).await?;
    let mut out = Vec::new();
    for n in notes {
        if n.system || n.author_username == current_user {
            continue;
        }
        if n.id <= last_note_id {
            continue;
        }
        out.push(Notification {
            id: format!("note:{}", n.id),
            kind: NotificationKind::Comment,
            at: n.created_at,
            author: n.author_username.clone(),
            summary: snippet(&n.body),
            url: format!("{}#note_{}", candidate.web_url, n.id),
            repo: candidate.repo_path.clone(),
            mr_iid: Some(candidate.iid.to_string()),
        });
    }
    Ok(out)
}

fn collect_new_todos(
    todos: &[RestTodo],
    last_todo_id: u64,
    current_user: &str,
    configured: &[RepoCfg],
) -> Vec<Notification> {
    let configured_paths: HashSet<&str> = configured.iter().map(|r| r.path.as_str()).collect();
    let mut out = Vec::new();

    for t in todos {
        if t.id <= last_todo_id {
            continue;
        }
        if t.author_username == current_user {
            continue;
        }
        let Some(kind) = todo_kind(&t.action_name) else {
            continue;
        };
        let url = t.target_url.clone().unwrap_or_default();
        let repo = repo_from_todo_url(&url, configured)
            .or_else(|| {
                // Fall back: match any configured repo prefix in the URL.
                configured_paths
                    .iter()
                    .find(|p| url.contains(*p))
                    .map(|p| (*p).to_string())
            })
            .unwrap_or_default();
        if !repo.is_empty() && !configured_paths.contains(repo.as_str()) {
            continue;
        }
        // Skip todos for repos outside our configured set when we could identify the repo.
        if !url.is_empty() && repo.is_empty() {
            continue;
        }

        let summary = snippet(
            t.body
                .as_deref()
                .or(t.target_title.as_deref())
                .unwrap_or("GitLab activity"),
        );
        out.push(Notification {
            id: format!("todo:{}", t.id),
            kind,
            at: t.created_at,
            author: t.author_username.clone(),
            summary,
            url,
            repo,
            mr_iid: mr_iid_from_url(&t.target_url.clone().unwrap_or_default()),
        });
    }
    out.sort_by_key(|n| n.at);
    out
}

/// Poll GitLab for new notifications and update the store.
/// Returns the number of newly appended items.
pub async fn poll(
    host: &str,
    repos: &[RepoCfg],
    current_user: &str,
    store: &mut NotificationStore,
) -> Result<usize> {
    if current_user.is_empty() || repos.is_empty() {
        return Ok(0);
    }

    let seeding = !store.seeded;
    let since = (Utc::now() - Duration::hours(24)).to_rfc3339();

    let discover_futures = repos.iter().map(|repo| {
        let host = host.to_string();
        let path = repo.path.clone();
        let user = current_user.to_string();
        let since = since.clone();
        async move {
            gitlab::discover_candidate_mrs(&host, &path, &user, &since)
                .await
                .map_err(|e| (path.clone(), e))
        }
    });
    let discover_results = futures::future::join_all(discover_futures).await;

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for result in discover_results {
        match result {
            Ok(mrs) => {
                for c in mrs {
                    let key = (c.project_id, c.iid);
                    if seen.insert(key) {
                        candidates.push(c);
                    }
                }
            }
            Err((path, e)) => {
                eprintln!("warning: discover MRs for {path} failed: {e}");
            }
        }
    }

    let mut new_items = Vec::new();
    let mut max_note_id = store.last_note_id;

    if seeding {
        let seed_futures = candidates.iter().map(|candidate| {
            let host = host.to_string();
            let project_id = candidate.project_id;
            let iid = candidate.iid;
            async move { gitlab::fetch_mr_latest_note_id(&host, project_id, iid).await }
        });
        for result in futures::future::join_all(seed_futures).await {
            if let Ok(Some(id)) = result {
                max_note_id = max_note_id.max(id);
            }
        }
    } else {
        let note_futures = candidates.iter().map(|candidate| {
            let host = host.to_string();
            let candidate = candidate.clone();
            let user = current_user.to_string();
            let last_note_id = store.last_note_id;
            async move { collect_new_notes(&host, &candidate, last_note_id, &user).await }
        });
        for result in futures::future::join_all(note_futures).await {
            match result {
                Ok(notes) => {
                    for n in &notes {
                        if let Some(id) = n.id.strip_prefix("note:") {
                            if let Ok(num) = id.parse::<u64>() {
                                max_note_id = max_note_id.max(num);
                            }
                        }
                    }
                    new_items.extend(notes);
                }
                Err(e) => eprintln!("warning: notes fetch failed: {e}"),
            }
        }
    }

    let todos = gitlab::fetch_pending_todos(host).await.unwrap_or_else(|e| {
        eprintln!("warning: todo list failed: {e}");
        Vec::new()
    });
    let max_todo_id = todos
        .iter()
        .map(|t| t.id)
        .fold(store.last_todo_id, u64::max);

    if !seeding {
        let todo_items = collect_new_todos(&todos, store.last_todo_id, current_user, repos);
        new_items.extend(todo_items);
    }

    store.last_note_id = max_note_id;
    store.last_todo_id = max_todo_id;
    store.seeded = true;

    let count = new_items.len();
    if !seeding {
        store.prepend(new_items);
        store.save()?;
    } else {
        store.save()?;
    }

    Ok(if seeding { 0 } else { count })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_truncates_long_text() {
        let long = "a".repeat(250);
        let s = snippet(&long);
        assert!(s.len() <= MAX_BODY);
        assert!(s.ends_with("..."));
    }

    #[test]
    fn prepend_dedups_and_caps() {
        let mut store = NotificationStore::default();
        let mk = |id: &str| Notification {
            id: id.to_string(),
            kind: NotificationKind::Comment,
            at: Utc::now(),
            author: "alice".to_string(),
            summary: "hi".to_string(),
            url: "http://x".to_string(),
            repo: "g/r".to_string(),
            mr_iid: Some("1".to_string()),
        };
        store.prepend((0..310).map(|i| mk(&format!("note:{i}"))).collect());
        assert_eq!(store.items.len(), MAX_ITEMS);
        store.prepend(vec![mk("note:999")]);
        assert_eq!(store.items.len(), MAX_ITEMS);
        assert_eq!(store.items[0].id, "note:999");
        store.prepend(vec![mk("note:999")]);
        assert_eq!(store.items[0].id, "note:999");
    }

    #[test]
    fn seeding_flag_defaults_false() {
        let store = NotificationStore::default();
        assert!(!store.seeded);
    }

    #[test]
    fn todo_kind_maps_actions() {
        assert_eq!(
            todo_kind("review_requested"),
            Some(NotificationKind::ReviewRequested)
        );
        assert_eq!(todo_kind("mentioned"), None);
    }
}
