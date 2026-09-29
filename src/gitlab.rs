use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::process::Command;

/// Percent-encode a GitLab project path for REST URLs (`group/repo` -> `group%2Frepo`).
fn encode_project_path(path: &str) -> String {
    path.replace('/', "%2F")
}

/// Percent-encode a query-string value.
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct MergeRequest {
    pub iid: String,
    pub title: String,
    pub web_url: String,
    pub author_name: String,
    pub author_username: String,
    pub approved_by: Vec<String>,
    pub approved: bool,
    pub draft: bool,
    pub has_conflicts: bool,
    pub notes_count: u32,
    pub last_note_author: Option<String>,
    pub last_note_at: Option<DateTime<Utc>>,
    pub ci: CiStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Latest activity among pipeline run, human note, or commit push.
    pub last_update: Option<(UpdateActivity, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateActivity {
    Build,
    Note,
    Push,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiStatus {
    Pass,
    Fail,
    InProgress,
    Cancelled,
    Other(String),
    None,
}

impl CiStatus {
    fn from_gitlab(status: Option<&str>) -> Self {
        match status {
            None => CiStatus::None,
            Some(s) => match s {
                "SUCCESS" => CiStatus::Pass,
                "FAILED" => CiStatus::Fail,
                "RUNNING"
                | "PENDING"
                | "CREATED"
                | "PREPARING"
                | "SCHEDULED"
                | "WAITING_FOR_RESOURCE" => CiStatus::InProgress,
                "CANCELED" | "CANCELLED" => CiStatus::Cancelled,
                other => CiStatus::Other(other.to_string()),
            },
        }
    }
}

const MR_QUERY: &str = r#"
query($fullPath: ID!) {
  project(fullPath: $fullPath) {
    mergeRequests(state: opened, sort: UPDATED_DESC, first: 50) {
      nodes {
        iid title webUrl draft conflicts createdAt updatedAt
        author { username name }
        approvedBy { nodes { username } }
        userNotesCount
        notes(last: 30) { nodes { author { username } createdAt system } }
        headPipeline { status user { username } createdAt }
        commits(last: 1) { nodes { author { username } committedDate } }
      }
    }
  }
}
"#;

// ----- Raw GraphQL response types -----

#[derive(Debug, Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ProjectData {
    project: Option<Project>,
}

#[derive(Debug, Deserialize)]
struct Project {
    #[serde(rename = "mergeRequests")]
    merge_requests: MrConnection,
}

#[derive(Debug, Deserialize)]
struct MrConnection {
    nodes: Vec<MrNode>,
}

#[derive(Debug, Deserialize)]
struct MrNode {
    iid: String,
    title: String,
    #[serde(rename = "webUrl")]
    web_url: String,
    draft: bool,
    conflicts: bool,
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
    #[serde(rename = "updatedAt")]
    updated_at: DateTime<Utc>,
    author: Option<UserRef>,
    #[serde(rename = "approvedBy")]
    approved_by: UserConnection,
    #[serde(rename = "userNotesCount")]
    user_notes_count: u32,
    notes: NoteConnection,
    #[serde(rename = "headPipeline")]
    head_pipeline: Option<Pipeline>,
    commits: CommitConnection,
}

#[derive(Debug, Deserialize)]
struct UserRef {
    username: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserConnection {
    nodes: Vec<UserRef>,
}

#[derive(Debug, Deserialize)]
struct NoteConnection {
    nodes: Vec<NoteNode>,
}

#[derive(Debug, Deserialize)]
struct NoteNode {
    author: Option<UserRef>,
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
    system: bool,
}

#[derive(Debug, Deserialize)]
struct Pipeline {
    status: Option<String>,
    user: Option<UserRef>,
    #[serde(rename = "createdAt")]
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct CommitConnection {
    nodes: Vec<CommitNode>,
}

#[derive(Debug, Deserialize)]
struct CommitNode {
    author: Option<UserRef>,
    #[serde(rename = "committedDate")]
    committed_date: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct CurrentUserData {
    #[serde(rename = "currentUser")]
    current_user: Option<UserRef>,
}

/// Run `glab api graphql` with the given query and variables, returning parsed JSON.
async fn run_graphql<T: for<'de> Deserialize<'de>>(
    host: &str,
    query: &str,
    vars: &[(&str, &str)],
) -> Result<T> {
    let mut cmd = Command::new("glab");
    cmd.arg("api")
        .arg("--hostname")
        .arg(host)
        .arg("graphql")
        .arg("-f")
        .arg(format!("query={query}"));
    for (k, v) in vars {
        cmd.arg("-f").arg(format!("{k}={v}"));
    }

    let output = cmd
        .output()
        .await
        .context("failed to spawn `glab`; is it installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("glab exited with {}: {}", output.status, stderr.trim());
    }

    let resp: GraphQlResponse<T> =
        serde_json::from_slice(&output.stdout).context("parsing glab GraphQL JSON")?;

    if !resp.errors.is_empty() {
        let msgs: Vec<String> = resp.errors.into_iter().map(|e| e.message).collect();
        anyhow::bail!("GraphQL errors: {}", msgs.join("; "));
    }

    resp.data.context("GraphQL response missing `data`")
}

/// Pick the most recent among pipeline run, human note, and latest commit.
fn latest_update(
    pipeline: &Option<Pipeline>,
    last_human: Option<&NoteNode>,
    last_commit: Option<&CommitNode>,
) -> Option<(UpdateActivity, String)> {
    let mut best: Option<(UpdateActivity, String, DateTime<Utc>)> = None;

    if let Some(p) = pipeline {
        if let (Some(user), Some(at)) = (&p.user, p.created_at) {
            best = later(best, (UpdateActivity::Build, user.username.clone(), at));
        }
    }
    if let Some(note) = last_human {
        if let Some(author) = &note.author {
            best = later(
                best,
                (
                    UpdateActivity::Note,
                    author.username.clone(),
                    note.created_at,
                ),
            );
        }
    }
    if let Some(commit) = last_commit {
        if let Some(author) = &commit.author {
            best = later(
                best,
                (
                    UpdateActivity::Push,
                    author.username.clone(),
                    commit.committed_date,
                ),
            );
        }
    }

    best.map(|(kind, user, _)| (kind, user))
}

fn later(
    current: Option<(UpdateActivity, String, DateTime<Utc>)>,
    candidate: (UpdateActivity, String, DateTime<Utc>),
) -> Option<(UpdateActivity, String, DateTime<Utc>)> {
    Some(match current {
        Some(prev) if prev.2 >= candidate.2 => prev,
        _ => candidate,
    })
}

/// Fetch the authenticated user's username (used for the mine-only filter).
pub async fn fetch_current_user(host: &str) -> Result<String> {
    let data: CurrentUserData =
        run_graphql(host, "query { currentUser { username } }", &[]).await?;
    data.current_user
        .map(|u| u.username)
        .context("could not determine current user")
}

/// Fetch open merge requests for a single project path.
pub async fn fetch_merge_requests(host: &str, full_path: &str) -> Result<Vec<MergeRequest>> {
    let data: ProjectData = run_graphql(host, MR_QUERY, &[("fullPath", full_path)]).await?;
    let project = data
        .project
        .with_context(|| format!("project `{full_path}` not found or not accessible"))?;

    let mrs = project
        .merge_requests
        .nodes
        .into_iter()
        .map(|n| {
            // Most recent non-system note = last human commenter.
            let last_human = n.notes.nodes.iter().rev().find(|note| !note.system);
            let (author, name) = match n.author {
                Some(u) => (u.username, u.name.unwrap_or_default()),
                None => (String::new(), String::new()),
            };
            let approved_by = n
                .approved_by
                .nodes
                .iter()
                .map(|u| u.username.clone())
                .collect::<Vec<_>>();
            MergeRequest {
                iid: n.iid,
                title: n.title,
                web_url: n.web_url,
                author_username: author,
                author_name: name,
                approved_by: approved_by.clone(),
                // `MergeRequest.approved` is true whenever approval requirements
                // are met, including when zero approvals are required. Treat an
                // MR as approved only when someone has actually approved it.
                approved: !approved_by.is_empty(),
                draft: n.draft,
                has_conflicts: n.conflicts,
                notes_count: n.user_notes_count,
                last_note_author: last_human
                    .and_then(|note| note.author.as_ref().map(|a| a.username.clone())),
                last_note_at: last_human.map(|note| note.created_at),
                ci: CiStatus::from_gitlab(
                    n.head_pipeline.as_ref().and_then(|p| p.status.as_deref()),
                ),
                created_at: n.created_at,
                updated_at: n.updated_at,
                last_update: latest_update(&n.head_pipeline, last_human, n.commits.nodes.first()),
            }
        })
        .collect::<Vec<_>>();

    // API already returns UPDATED_DESC; re-sort defensively.
    let mut mrs = mrs;
    mrs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    Ok(mrs)
}

// ----- REST API (via `glab api`) -----

/// Run `glab api --hostname <host> <path>` and parse JSON.
async fn run_rest<T: for<'de> Deserialize<'de>>(host: &str, path: &str) -> Result<T> {
    let output = Command::new("glab")
        .arg("api")
        .arg("--hostname")
        .arg(host)
        .arg(path)
        .output()
        .await
        .context("failed to spawn `glab`; is it installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("glab exited with {}: {}", output.status, stderr.trim());
    }

    serde_json::from_slice(&output.stdout).context("parsing glab REST JSON")
}

#[derive(Debug, Clone)]
pub struct MrCandidate {
    pub project_id: u64,
    pub iid: u32,
    pub web_url: String,
    pub repo_path: String,
}

#[derive(Debug, Clone)]
pub struct RestNote {
    pub id: u64,
    pub author_username: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub system: bool,
}

#[derive(Debug, Clone)]
pub struct RestTodo {
    pub id: u64,
    pub action_name: String,
    pub author_username: String,
    pub body: Option<String>,
    pub target_url: Option<String>,
    pub target_title: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct RestMr {
    iid: u32,
    project_id: u64,
    web_url: String,
}

#[derive(Debug, Deserialize)]
struct RestNoteRaw {
    id: u64,
    body: String,
    system: bool,
    author: Option<UserRef>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct RestTodoRaw {
    id: u64,
    action_name: String,
    body: Option<String>,
    target_url: Option<String>,
    author: Option<UserRef>,
    created_at: DateTime<Utc>,
    target: Option<RestTodoTarget>,
}

#[derive(Debug, Deserialize)]
struct RestTodoTarget {
    title: Option<String>,
}

const MR_ROLES: &[(&str, &str)] = &[
    ("author_username", "author"),
    ("reviewer_username", "reviewer"),
    ("assignee_username", "assignee"),
];

/// Open MRs in `repo_path` where `username` has one of author/reviewer/assignee roles.
pub async fn discover_candidate_mrs(
    host: &str,
    repo_path: &str,
    username: &str,
    since: &str,
) -> Result<Vec<MrCandidate>> {
    let encoded = encode_project_path(repo_path);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for (param, _) in MR_ROLES {
        let path = format!(
            "projects/{encoded}/merge_requests?state=opened&{param}={}&updated_after={}\
             &per_page=50&order_by=updated_at&sort=desc",
            encode_query(username),
            encode_query(since),
        );
        let list: Vec<RestMr> = match run_rest(host, &path).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        for mr in list {
            let key = (mr.project_id, mr.iid);
            if seen.insert(key) {
                out.push(MrCandidate {
                    project_id: mr.project_id,
                    iid: mr.iid,
                    web_url: mr.web_url,
                    repo_path: repo_path.to_string(),
                });
            }
        }
    }

    Ok(out)
}

/// Notes on an MR, oldest first.
pub async fn fetch_mr_notes(host: &str, project_id: u64, iid: u32) -> Result<Vec<RestNote>> {
    let path = format!(
        "projects/{project_id}/merge_requests/{iid}/notes\
         ?sort=asc&order_by=created_at&per_page=100"
    );
    let raw: Vec<RestNoteRaw> = run_rest(host, &path).await?;
    Ok(raw
        .into_iter()
        .map(|n| RestNote {
            id: n.id,
            author_username: n
                .author
                .map(|a| a.username)
                .unwrap_or_else(|| "unknown".to_string()),
            body: n.body,
            created_at: n.created_at,
            system: n.system,
        })
        .collect())
}

/// Most recent note id on an MR, if any (used to seed the notification cursor).
pub async fn fetch_mr_latest_note_id(host: &str, project_id: u64, iid: u32) -> Result<Option<u64>> {
    let path = format!(
        "projects/{project_id}/merge_requests/{iid}/notes\
         ?sort=desc&order_by=created_at&per_page=1"
    );
    let raw: Vec<RestNoteRaw> = run_rest(host, &path).await?;
    Ok(raw.first().map(|n| n.id))
}

/// Pending todos for the authenticated user.
pub async fn fetch_pending_todos(host: &str) -> Result<Vec<RestTodo>> {
    let raw: Vec<RestTodoRaw> = run_rest(host, "todos?state=pending&per_page=50").await?;
    Ok(raw
        .into_iter()
        .map(|t| RestTodo {
            id: t.id,
            action_name: t.action_name,
            author_username: t
                .author
                .map(|a| a.username)
                .unwrap_or_else(|| "unknown".to_string()),
            body: t.body,
            target_url: t.target_url,
            target_title: t.target.and_then(|tg| tg.title),
            created_at: t.created_at,
        })
        .collect())
}
