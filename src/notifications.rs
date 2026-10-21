use std::collections::{HashMap, HashSet};
use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::gitlab::{self, Discussion, MrCandidate, RestTodo};

pub const MAX_ITEMS: usize = 300;
const MAX_BODY: usize = 200;

const TODO_ACTIONS: &[&str] = &["review_requested", "review_submitted", "build_failed"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Comment,
    /// Someone replied in a thread where the current user had written a note.
    CommentReply,
    ReviewSubmitted,
    ReviewRequested,
    PipelineFailed,
}

impl NotificationKind {
    pub fn verb(self) -> &'static str {
        match self {
            NotificationKind::Comment => "commented",
            NotificationKind::CommentReply => "replied to your comment",
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
    /// Cursor for discussion-thread replies has been initialized.
    /// Missing on stores written before reply tracking existed.
    #[serde(default)]
    pub replies_seeded: bool,
    pub items: Vec<Notification>,
    /// `updated_at` of each candidate MR when its notes and discussions were last
    /// read, keyed by `(project_id, iid)`. Unchanged MRs are not refetched.
    #[serde(skip)]
    pub mr_updated_at: HashMap<(u64, u32), DateTime<Utc>>,
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self {
            last_note_id: 0,
            last_todo_id: 0,
            seeded: false,
            replies_seeded: false,
            items: Vec::new(),
            mr_updated_at: HashMap::new(),
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

fn user_wrote_in_thread(discussion: &Discussion, current_user: &str) -> bool {
    discussion
        .notes
        .iter()
        .any(|n| !n.system && n.author_username == current_user)
}

/// Highest note id in threads the user has written in.
fn participated_thread_high_water(discussions: &[Discussion], current_user: &str) -> u64 {
    discussions
        .iter()
        .filter(|d| !d.individual_note && user_wrote_in_thread(d, current_user))
        .flat_map(|d| d.notes.iter().map(|n| n.id))
        .max()
        .unwrap_or(0)
}

/// Replies, in threads the user has written in, that the notes API does not return.
fn collect_thread_replies(
    discussions: &[Discussion],
    candidate: &MrCandidate,
    last_note_id: u64,
    current_user: &str,
) -> Vec<Notification> {
    let mut out = Vec::new();
    for discussion in discussions {
        if discussion.individual_note {
            continue;
        }
        let Some(my_first) = discussion
            .notes
            .iter()
            .filter(|n| !n.system && n.author_username == current_user)
            .map(|n| n.id)
            .min()
        else {
            continue;
        };
        for n in &discussion.notes {
            if n.system || n.author_username == current_user || n.note_type.is_none() {
                continue;
            }
            if n.id <= last_note_id || n.id <= my_first {
                continue;
            }
            out.push(Notification {
                id: format!("note:{}", n.id),
                kind: NotificationKind::CommentReply,
                at: n.created_at,
                author: n.author_username.clone(),
                summary: snippet(&n.body),
                url: format!("{}#note_{}", candidate.web_url, n.id),
                repo: candidate.repo_path.clone(),
                mr_iid: Some(candidate.iid.to_string()),
            });
        }
    }
    out
}

fn collect_new_todos(
    todos: &[RestTodo],
    last_todo_id: u64,
    current_user: &str,
) -> Vec<Notification> {
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
        let repo = gitlab::repo_path_from_url(&url).unwrap_or_default();
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

/// Candidates that are new or whose `updated_at` moved since they were last read.
fn changed_candidates(
    candidates: &[MrCandidate],
    seen: &HashMap<(u64, u32), DateTime<Utc>>,
) -> Vec<MrCandidate> {
    candidates
        .iter()
        .filter(|c| seen.get(&c.key()) != Some(&c.updated_at))
        .cloned()
        .collect()
}

/// Poll GitLab for new notifications and update the store.
/// Returns newly appended items (empty during the initial seeding poll).
pub async fn poll(
    host: &str,
    current_user: &str,
    store: &mut NotificationStore,
) -> Result<Vec<Notification>> {
    if current_user.is_empty() {
        return Ok(Vec::new());
    }

    let seeding = !store.seeded;
    let since = (Utc::now() - Duration::hours(24)).to_rfc3339();

    let candidates = gitlab::discover_candidate_mrs(host, current_user, &since)
        .await
        .unwrap_or_default();
    let changed = changed_candidates(&candidates, &store.mr_updated_at);
    let current: HashSet<(u64, u32)> = candidates.iter().map(MrCandidate::key).collect();
    store.mr_updated_at.retain(|key, _| current.contains(key));

    let mut new_items = Vec::new();
    let mut max_note_id = store.last_note_id;
    let mut notes_read = HashSet::new();

    if seeding {
        let seed_futures = changed.iter().map(|candidate| {
            let host = host.to_string();
            let (project_id, iid) = candidate.key();
            async move {
                let result = gitlab::fetch_mr_latest_note_id(&host, project_id, iid).await;
                ((project_id, iid), result)
            }
        });
        for (key, result) in futures::future::join_all(seed_futures).await {
            let Ok(latest) = result else {
                continue;
            };
            notes_read.insert(key);
            if let Some(id) = latest {
                max_note_id = max_note_id.max(id);
            }
        }
    } else {
        let note_futures = changed.iter().map(|candidate| {
            let host = host.to_string();
            let candidate = candidate.clone();
            let user = current_user.to_string();
            let last_note_id = store.last_note_id;
            async move {
                let result = collect_new_notes(&host, &candidate, last_note_id, &user).await;
                (candidate.key(), result)
            }
        });
        for (key, result) in futures::future::join_all(note_futures).await {
            let Ok(notes) = result else {
                continue;
            };
            notes_read.insert(key);
            for n in &notes {
                if let Some(id) = n.id.strip_prefix("note:") {
                    if let Ok(num) = id.parse::<u64>() {
                        max_note_id = max_note_id.max(num);
                    }
                }
            }
            new_items.extend(notes);
        }
    }

    let discussion_futures = changed.iter().map(|candidate| {
        let host = host.to_string();
        let candidate = candidate.clone();
        async move {
            gitlab::fetch_mr_discussions(&host, candidate.project_id, candidate.iid)
                .await
                .map(|discussions| (candidate, discussions))
        }
    });
    let mut reply_high_water = 0u64;
    let mut replies = Vec::new();
    for result in futures::future::join_all(discussion_futures).await {
        let Ok((candidate, discussions)) = result else {
            continue;
        };
        reply_high_water =
            reply_high_water.max(participated_thread_high_water(&discussions, current_user));
        if !seeding && store.replies_seeded {
            replies.extend(collect_thread_replies(
                &discussions,
                &candidate,
                store.last_note_id,
                current_user,
            ));
        }
        if notes_read.contains(&candidate.key()) {
            store
                .mr_updated_at
                .insert(candidate.key(), candidate.updated_at);
        }
    }
    if seeding || !store.replies_seeded {
        max_note_id = max_note_id.max(reply_high_water);
        store.replies_seeded = true;
    } else {
        {
            let reply_ids: HashSet<&str> = replies.iter().map(|n| n.id.as_str()).collect();
            new_items.retain(|n| !reply_ids.contains(n.id.as_str()));
        }
        for n in &replies {
            if let Some(id) = n.id.strip_prefix("note:") {
                if let Ok(num) = id.parse::<u64>() {
                    max_note_id = max_note_id.max(num);
                }
            }
        }
        max_note_id = max_note_id.max(reply_high_water);
        new_items.extend(replies);
    }

    let todos = gitlab::fetch_pending_todos(host).await.unwrap_or_default();
    let max_todo_id = todos
        .iter()
        .map(|t| t.id)
        .fold(store.last_todo_id, u64::max);

    if !seeding {
        let todo_items = collect_new_todos(&todos, store.last_todo_id, current_user);
        new_items.extend(todo_items);
    }

    store.last_note_id = max_note_id;
    store.last_todo_id = max_todo_id;
    store.seeded = true;

    if !seeding {
        let appended = new_items;
        store.prepend(appended.clone());
        store.save()?;
        Ok(appended)
    } else {
        store.save()?;
        Ok(Vec::new())
    }
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

    #[test]
    fn repo_path_from_url_reads_the_project_before_the_dash() {
        assert_eq!(
            gitlab::repo_path_from_url(
                "https://git.example.com/group/ext-service/-/merge_requests/12"
            )
            .as_deref(),
            Some("group/ext-service")
        );
        assert_eq!(
            gitlab::repo_path_from_url("https://git.example.com/group/other/-/pipelines/9")
                .as_deref(),
            Some("group/other")
        );
    }

    #[test]
    fn todos_include_projects_outside_config() {
        let todos = vec![RestTodo {
            id: 5,
            action_name: "review_requested".into(),
            author_username: "bob".into(),
            body: None,
            target_url: Some("https://git.example.com/group/other/-/merge_requests/3".into()),
            target_title: Some("Add thing".into()),
            created_at: Utc::now(),
        }];
        let items = collect_new_todos(&todos, 0, "alice");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].repo, "group/other");
        assert_eq!(items[0].mr_iid.as_deref(), Some("3"));
    }

    #[test]
    fn replies_seeded_defaults_when_missing() {
        let json = r#"{"last_note_id":1,"last_todo_id":2,"seeded":true,"items":[]}"#;
        let store: NotificationStore = serde_json::from_str(json).unwrap();
        assert!(store.seeded);
        assert!(!store.replies_seeded);
    }

    fn thread_note(id: u64, author: &str, note_type: Option<&str>) -> gitlab::DiscussionNote {
        gitlab::DiscussionNote {
            id,
            note_type: note_type.map(str::to_string),
            author_username: author.to_string(),
            body: format!("body {id}"),
            created_at: Utc::now(),
            system: false,
        }
    }

    fn candidate() -> MrCandidate {
        MrCandidate {
            project_id: 1,
            iid: 7,
            web_url: "https://git.example.com/group/repo/-/merge_requests/7".to_string(),
            repo_path: "group/repo".to_string(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn changed_candidates_skip_unchanged_merge_requests() {
        let unchanged = candidate();
        let mut moved = candidate();
        moved.iid = 8;
        let mut new = candidate();
        new.iid = 9;
        let seen = HashMap::from([
            (unchanged.key(), unchanged.updated_at),
            (moved.key(), moved.updated_at - Duration::minutes(1)),
        ]);

        let changed = changed_candidates(&[unchanged, moved, new], &seen);
        let iids: Vec<u32> = changed.iter().map(|c| c.iid).collect();
        assert_eq!(iids, vec![8, 9]);
    }

    #[test]
    fn thread_replies_include_responses_after_my_review_comment() {
        let discussions = vec![Discussion {
            individual_note: false,
            notes: vec![
                thread_note(10, "me", Some("DiffNote")),
                thread_note(11, "alice", Some("DiscussionNote")),
                thread_note(12, "bob", Some("DiffNote")),
            ],
        }];
        let replies = collect_thread_replies(&discussions, &candidate(), 0, "me");
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].kind, NotificationKind::CommentReply);
        assert_eq!(replies[0].author, "alice");
        assert_eq!(replies[0].id, "note:11");
        assert_eq!(
            replies[1].url,
            "https://git.example.com/group/repo/-/merge_requests/7#note_12"
        );
        assert_eq!(
            NotificationKind::CommentReply.verb(),
            "replied to your comment"
        );
        assert_eq!(participated_thread_high_water(&discussions, "me"), 12);
    }

    #[test]
    fn thread_replies_skip_notes_before_i_wrote_and_unrelated_threads() {
        let discussions = vec![
            Discussion {
                individual_note: false,
                notes: vec![
                    thread_note(4, "alice", Some("DiffNote")),
                    thread_note(8, "me", Some("DiscussionNote")),
                    thread_note(9, "alice", Some("DiscussionNote")),
                    thread_note(20, "me", Some("DiscussionNote")),
                ],
            },
            Discussion {
                individual_note: false,
                notes: vec![thread_note(30, "carol", Some("DiffNote"))],
            },
            Discussion {
                individual_note: true,
                notes: vec![thread_note(40, "dave", None)],
            },
        ];
        let replies = collect_thread_replies(&discussions, &candidate(), 0, "me");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, "note:9");
        assert_eq!(participated_thread_high_water(&discussions, "me"), 20);
        assert_eq!(participated_thread_high_water(&discussions, "nobody"), 0);
    }

    #[test]
    fn thread_replies_skip_root_comments_and_old_cursor() {
        let mut system = thread_note(15, "gitlab", Some("DiscussionNote"));
        system.system = true;
        let discussions = vec![Discussion {
            individual_note: false,
            notes: vec![
                thread_note(10, "me", Some("DiffNote")),
                thread_note(11, "alice", None),
                thread_note(12, "alice", Some("DiscussionNote")),
                system,
            ],
        }];
        let replies = collect_thread_replies(&discussions, &candidate(), 0, "me");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, "note:12");

        let already_seen = collect_thread_replies(&discussions, &candidate(), 12, "me");
        assert!(already_seen.is_empty());
    }
}
