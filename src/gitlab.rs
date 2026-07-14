use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct MergeRequest {
    pub iid: String,
    pub title: String,
    pub web_url: String,
    pub author_name: String,
    pub author_username: String,
    pub approved: bool,
    pub draft: bool,
    pub notes_count: u32,
    pub last_note_author: Option<String>,
    pub last_note_at: Option<DateTime<Utc>>,
    pub ci: CiStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiStatus {
    Pass,
    Fail,
    InProgress,
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
                "RUNNING" | "PENDING" | "CREATED" | "PREPARING" | "SCHEDULED"
                | "WAITING_FOR_RESOURCE" => CiStatus::InProgress,
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
        iid title webUrl draft createdAt updatedAt
        author { username name }
        approved
        userNotesCount
        notes(last: 30) { nodes { author { username } createdAt system } }
        headPipeline { status }
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
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
    #[serde(rename = "updatedAt")]
    updated_at: DateTime<Utc>,
    author: Option<UserRef>,
    approved: bool,
    #[serde(rename = "userNotesCount")]
    user_notes_count: u32,
    notes: NoteConnection,
    #[serde(rename = "headPipeline")]
    head_pipeline: Option<Pipeline>,
}

#[derive(Debug, Deserialize)]
struct UserRef {
    username: String,
    #[serde(default)]
    name: Option<String>,
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
            let last_human = n
                .notes
                .nodes
                .iter()
                .rev()
                .find(|note| !note.system);
            let (author, name) = match n.author {
                Some(u) => (u.username, u.name.unwrap_or_default()),
                None => (String::new(), String::new()),
            };
            MergeRequest {
                iid: n.iid,
                title: n.title,
                web_url: n.web_url,
                author_username: author,
                author_name: name,
                approved: n.approved,
                draft: n.draft,
                notes_count: n.user_notes_count,
                last_note_author: last_human
                    .and_then(|note| note.author.as_ref().map(|a| a.username.clone())),
                last_note_at: last_human.map(|note| note.created_at),
                ci: CiStatus::from_gitlab(
                    n.head_pipeline.as_ref().and_then(|p| p.status.as_deref()),
                ),
                created_at: n.created_at,
                updated_at: n.updated_at,
            }
        })
        .collect::<Vec<_>>();

    // API already returns UPDATED_DESC; re-sort defensively.
    let mut mrs = mrs;
    mrs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    Ok(mrs)
}
