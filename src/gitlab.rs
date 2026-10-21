use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::process::Command;

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
    pub source_branch: String,
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
query GlabFeedMergeRequests($fullPath: ID!) {
  project(fullPath: $fullPath) {
    mergeRequests(state: opened, sort: UPDATED_DESC, first: 50) {
      nodes {
        iid title webUrl draft conflicts createdAt updatedAt sourceBranch
        author { username name }
        approvedBy { nodes { username } }
        userNotesCount
        headPipeline { status user { username } createdAt }
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
    #[serde(rename = "sourceBranch")]
    source_branch: String,
    author: Option<UserRef>,
    #[serde(rename = "approvedBy")]
    approved_by: UserConnection,
    #[serde(rename = "userNotesCount")]
    user_notes_count: u32,
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
    /// `commit` on the "added N commits" system note GitLab writes for each push.
    #[serde(rename = "systemNoteIconName", default)]
    system_note_icon_name: Option<String>,
}

impl NoteNode {
    fn is_push(&self) -> bool {
        self.system && self.system_note_icon_name.as_deref() == Some("commit")
    }
}

#[derive(Debug, Deserialize)]
struct Pipeline {
    status: Option<String>,
    user: Option<UserRef>,
    #[serde(rename = "createdAt")]
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct CurrentUserData {
    #[serde(rename = "currentUser")]
    current_user: Option<UserRef>,
}

/// Run `glab api --hostname <host> <args>` and return stdout.
///
/// Every GitLab request goes through here. With `--debug`, each call logs
/// `label`, whether glab paginates (possibly several HTTP requests), the
/// duration, and the outcome.
pub async fn glab_api(host: &str, args: &[String], label: &str) -> Result<Vec<u8>> {
    let paginated = args.iter().any(|a| a == "--paginate");
    let started = Instant::now();
    let result = run_glab(host, args).await;
    log::debug!(
        "{}",
        request_line(label, paginated, started.elapsed(), &result)
    );
    result
}

async fn run_glab(host: &str, args: &[String]) -> Result<Vec<u8>> {
    let output = Command::new("glab")
        .arg("api")
        .arg("--hostname")
        .arg(host)
        .args(args)
        .output()
        .await
        .context("failed to spawn `glab`; is it installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("glab exited with {}: {}", output.status, stderr.trim());
    }
    Ok(output.stdout)
}

fn request_line(
    label: &str,
    paginated: bool,
    elapsed: Duration,
    result: &Result<Vec<u8>>,
) -> String {
    let paginated = if paginated { " paginated" } else { "" };
    let ms = elapsed.as_millis();
    match result {
        Ok(body) => format!("{label}{paginated} {ms}ms ok {}B", body.len()),
        Err(e) => format!(
            "{label}{paginated} {ms}ms failed: {}",
            format!("{e:#}").replace('\n', " ")
        ),
    }
}

/// Name after the `query` keyword, e.g. `GlabFeedMergeRequests`.
fn graphql_operation_name(query: &str) -> Option<&str> {
    let rest = query.trim_start().strip_prefix("query")?;
    let name = rest
        .trim_start()
        .split(|c: char| c == '(' || c == '{' || c.is_whitespace())
        .next()?;
    (!name.is_empty()).then_some(name)
}

/// Run `glab api graphql` with the given query and variables, returning parsed JSON.
async fn run_graphql<T: for<'de> Deserialize<'de>>(
    host: &str,
    query: &str,
    vars: &[(&str, &str)],
) -> Result<T> {
    let mut args = vec!["graphql".to_string(), "-f".into(), format!("query={query}")];
    let mut label = format!(
        "graphql {}",
        graphql_operation_name(query).unwrap_or("anonymous")
    );
    for (k, v) in vars {
        args.push("-f".into());
        args.push(format!("{k}={v}"));
        label.push_str(&format!(" {k}={v}"));
    }

    let body = glab_api(host, &args, &label).await?;
    let resp: GraphQlResponse<T> = serde_json::from_slice(&body)
        .context("parsing glab GraphQL JSON")
        .inspect_err(|e| log::debug!("{label} failed: {e:#}"))?;

    if !resp.errors.is_empty() {
        let msgs: Vec<String> = resp.errors.into_iter().map(|e| e.message).collect();
        log::debug!("{label} failed: GraphQL errors: {}", msgs.join("; "));
        anyhow::bail!("GraphQL errors: {}", msgs.join("; "));
    }

    resp.data.context("GraphQL response missing `data`")
}

/// Pick the most recent among pipeline run, human note, and push.
fn latest_update(
    pipeline: &Option<Pipeline>,
    last_human: Option<&NoteNode>,
    last_push: Option<&NoteNode>,
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
    if let Some(push) = last_push {
        if let Some(author) = &push.author {
            best = later(
                best,
                (
                    UpdateActivity::Push,
                    author.username.clone(),
                    push.created_at,
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
    let data: CurrentUserData = run_graphql(
        host,
        "query GlabFeedCurrentUser { currentUser { username } }",
        &[],
    )
    .await?;
    data.current_user
        .map(|u| u.username)
        .context("could not determine current user")
}

/// Fetch open merge requests for a single project path.
///
/// The list query does not ask for notes. Notes are loaded only for merge
/// requests whose `updated_at` differs from `previous` (including new ones);
/// the rest reuse the previous comment and push activity.
pub async fn fetch_merge_requests(
    host: &str,
    full_path: &str,
    previous: &[MergeRequest],
) -> Result<Vec<MergeRequest>> {
    let data: ProjectData = run_graphql(host, MR_QUERY, &[("fullPath", full_path)]).await?;
    let project = data
        .project
        .with_context(|| format!("project `{full_path}` not found or not accessible"))?;

    let mut drafts: Vec<Draft> = project
        .merge_requests
        .nodes
        .into_iter()
        .map(draft_from_node)
        .collect();
    let need: Vec<String> = drafts
        .iter()
        .filter(|draft| needs_notes(previous, &draft.mr.iid, draft.mr.updated_at))
        .map(|draft| draft.mr.iid.clone())
        .collect();
    for draft in &mut drafts {
        if !needs_notes(previous, &draft.mr.iid, draft.mr.updated_at) {
            copy_activity(previous, &mut draft.mr);
        }
    }
    if !need.is_empty() {
        let notes = fetch_recent_notes(host, full_path, &need).await?;
        for draft in &mut drafts {
            if let Some(nodes) = notes.get(&draft.mr.iid) {
                apply_notes(&mut draft.mr, &draft.pipeline, nodes);
            }
        }
    }

    let mut mrs: Vec<MergeRequest> = drafts.into_iter().map(|draft| draft.mr).collect();
    mrs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(mrs)
}

struct Draft {
    mr: MergeRequest,
    pipeline: Option<Pipeline>,
}

fn draft_from_node(n: MrNode) -> Draft {
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
    let mr = MergeRequest {
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
        last_note_author: None,
        last_note_at: None,
        ci: CiStatus::from_gitlab(n.head_pipeline.as_ref().and_then(|p| p.status.as_deref())),
        created_at: n.created_at,
        updated_at: n.updated_at,
        source_branch: n.source_branch,
        last_update: latest_update(&n.head_pipeline, None, None),
    };
    Draft {
        mr,
        pipeline: n.head_pipeline,
    }
}

fn needs_notes(previous: &[MergeRequest], iid: &str, updated_at: DateTime<Utc>) -> bool {
    !previous
        .iter()
        .any(|mr| mr.iid == iid && mr.updated_at == updated_at)
}

fn copy_activity(previous: &[MergeRequest], mr: &mut MergeRequest) {
    let Some(prev) = previous
        .iter()
        .find(|prev| prev.iid == mr.iid && prev.updated_at == mr.updated_at)
    else {
        return;
    };
    mr.last_note_author = prev.last_note_author.clone();
    mr.last_note_at = prev.last_note_at;
    mr.last_update = prev.last_update.clone();
}

fn apply_notes(mr: &mut MergeRequest, pipeline: &Option<Pipeline>, notes: &[NoteNode]) {
    let last_human = notes.iter().rev().find(|note| !note.system);
    let last_push = notes.iter().rev().find(|note| note.is_push());
    mr.last_note_author =
        last_human.and_then(|note| note.author.as_ref().map(|a| a.username.clone()));
    mr.last_note_at = last_human.map(|note| note.created_at);
    mr.last_update = latest_update(pipeline, last_human, last_push);
}

/// One `mergeRequest` alias costs about 10 complexity points. Fifty of them
/// score 501, past the authenticated limit of 250. The same notes on one
/// `mergeRequests(iids:)` connection score about 50, so a page of 50 stays
/// inside the limit.
const NOTES_PAGE: usize = 50;

async fn fetch_recent_notes(
    host: &str,
    full_path: &str,
    iids: &[String],
) -> Result<HashMap<String, Vec<NoteNode>>> {
    let iids: Vec<&str> = iids
        .iter()
        .map(String::as_str)
        .filter(|iid| iid.chars().all(|c| c.is_ascii_digit()))
        .collect();
    let mut out = HashMap::new();
    for chunk in iids.chunks(NOTES_PAGE) {
        let data: NotesData =
            run_graphql(host, &notes_query(chunk), &[("fullPath", full_path)]).await?;
        let Some(project) = data.project else {
            continue;
        };
        for mr in project.merge_requests.nodes {
            out.insert(mr.iid, mr.notes.nodes);
        }
    }
    Ok(out)
}

fn notes_query(iids: &[&str]) -> String {
    let list = iids
        .iter()
        .map(|iid| format!("\"{iid}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "query GlabFeedMergeRequestNotes($fullPath: ID!) {{ \
         project(fullPath: $fullPath) {{ \
         mergeRequests(iids: [{list}], first: {first}) {{ \
         nodes {{ iid notes(last: 30) {{ nodes {{ author {{ username }} createdAt system systemNoteIconName }} }} }} \
         }} }} }}",
        first = iids.len()
    )
}

#[derive(Deserialize)]
struct NotesData {
    project: Option<NotesProject>,
}

#[derive(Deserialize)]
struct NotesProject {
    #[serde(rename = "mergeRequests")]
    merge_requests: NotesConnection,
}

#[derive(Deserialize)]
struct NotesConnection {
    nodes: Vec<NotesMr>,
}

#[derive(Deserialize)]
struct NotesMr {
    iid: String,
    notes: NoteConnection,
}

// ----- REST API (via `glab api`) -----

/// Run `glab api --hostname <host> <path>` and parse JSON.
async fn run_rest<T: for<'de> Deserialize<'de>>(host: &str, path: &str) -> Result<T> {
    run_rest_inner(host, path, false).await
}

/// Same as [`run_rest`], following GitLab pagination and concatenating array pages.
async fn run_rest_paginated<T: for<'de> Deserialize<'de>>(host: &str, path: &str) -> Result<T> {
    run_rest_inner(host, path, true).await
}

async fn run_rest_inner<T: for<'de> Deserialize<'de>>(
    host: &str,
    path: &str,
    paginate: bool,
) -> Result<T> {
    let mut args = Vec::new();
    if paginate {
        args.push("--paginate".to_string());
    }
    args.push(path.to_string());
    let label = format!("rest {path}");

    let body = glab_api(host, &args, &label).await?;
    serde_json::from_slice(&body)
        .context("parsing glab REST JSON")
        .inspect_err(|e| log::debug!("{label} failed: {e:#}"))
}

/// Project path from a GitLab URL: everything before `/-/`.
pub fn repo_path_from_url(url: &str) -> Option<String> {
    let after_host = url.split("://").nth(1)?;
    let path = after_host.split('/').skip(1).collect::<Vec<_>>().join("/");
    let repo = path.split("/-/").next()?.trim_matches('/');
    if repo.is_empty() {
        None
    } else {
        Some(repo.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct MrCandidate {
    pub project_id: u64,
    pub iid: u32,
    pub web_url: String,
    pub repo_path: String,
    pub updated_at: DateTime<Utc>,
}

impl MrCandidate {
    pub fn key(&self) -> (u64, u32) {
        (self.project_id, self.iid)
    }
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
    updated_at: DateTime<Utc>,
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

/// Open MRs the user authored, reviews, or is assigned, in every accessible
/// project, updated at or after `since`.
pub async fn discover_candidate_mrs(
    host: &str,
    username: &str,
    since: &str,
) -> Result<Vec<MrCandidate>> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for (param, _) in MR_ROLES {
        let path = format!(
            "merge_requests?scope=all&state=opened&{param}={}&updated_after={}\
             &per_page=100&order_by=updated_at&sort=desc",
            encode_query(username),
            encode_query(since),
        );
        let list: Vec<RestMr> = match run_rest_paginated(host, &path).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        for mr in list {
            let key = (mr.project_id, mr.iid);
            let Some(repo_path) = repo_path_from_url(&mr.web_url) else {
                continue;
            };
            if seen.insert(key) {
                out.push(MrCandidate {
                    project_id: mr.project_id,
                    iid: mr.iid,
                    web_url: mr.web_url,
                    repo_path,
                    updated_at: mr.updated_at,
                });
            }
        }
    }

    Ok(out)
}

/// Pending todos for the authenticated user, newest id first.
pub async fn fetch_pending_todos(host: &str) -> Result<Vec<RestTodo>> {
    let raw: Vec<RestTodoRaw> = run_rest(
        host,
        "todos?state=pending&per_page=50&order_by=id&sort=desc",
    )
    .await?;
    Ok(map_todos(raw))
}

/// Id of the newest pending todo, if the user has any.
pub async fn fetch_newest_pending_todo_id(host: &str) -> Result<Option<u64>> {
    let raw: Vec<RestTodoRaw> =
        run_rest(host, "todos?state=pending&per_page=1&order_by=id&sort=desc").await?;
    Ok(raw.first().map(|todo| todo.id))
}

fn map_todos(raw: Vec<RestTodoRaw>) -> Vec<RestTodo> {
    raw.into_iter()
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
        .collect()
}

#[derive(Debug, Clone)]
pub struct Discussion {
    pub individual_note: bool,
    pub notes: Vec<DiscussionNote>,
}

#[derive(Debug, Clone)]
pub struct DiscussionNote {
    pub id: u64,
    /// `DiffNote` / `DiscussionNote` for a reply. `None` for a standalone comment
    /// or the root note of a thread.
    pub note_type: Option<String>,
    pub author_username: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub system: bool,
}

#[derive(Debug, Deserialize)]
struct RestDiscussionRaw {
    individual_note: bool,
    notes: Vec<RestDiscussionNoteRaw>,
}

#[derive(Debug, Deserialize)]
struct RestDiscussionNoteRaw {
    id: u64,
    #[serde(default, rename = "type")]
    note_type: Option<String>,
    body: String,
    system: bool,
    author: Option<UserRef>,
    created_at: DateTime<Utc>,
}

const DISCUSSION_PAGE_SIZE: usize = 100;
const DISCUSSION_MAX_PAGES: u32 = 10;

/// Discussion threads on an MR. Standalone comments and replies both arrive here.
pub async fn fetch_mr_discussions(
    host: &str,
    project_id: u64,
    iid: u32,
) -> Result<Vec<Discussion>> {
    let mut all = Vec::new();
    for page in 1..=DISCUSSION_MAX_PAGES {
        let path = format!(
            "projects/{project_id}/merge_requests/{iid}/discussions\
             ?per_page={DISCUSSION_PAGE_SIZE}&page={page}"
        );
        let raw: Vec<RestDiscussionRaw> = run_rest(host, &path).await?;
        let count = raw.len();
        all.extend(raw.into_iter().map(|d| {
            Discussion {
                individual_note: d.individual_note,
                notes: d
                    .notes
                    .into_iter()
                    .map(|n| DiscussionNote {
                        id: n.id,
                        note_type: n.note_type,
                        author_username: n
                            .author
                            .map(|a| a.username)
                            .unwrap_or_else(|| "unknown".to_string()),
                        body: n.body,
                        created_at: n.created_at,
                        system: n.system,
                    })
                    .collect(),
            }
        }));
        if count < DISCUSSION_PAGE_SIZE {
            break;
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn note(author: &str, minutes_ago: i64, system: bool, icon: Option<&str>) -> NoteNode {
        NoteNode {
            author: Some(UserRef {
                username: author.into(),
                name: None,
            }),
            created_at: Utc::now() - Duration::minutes(minutes_ago),
            system,
            system_note_icon_name: icon.map(str::to_string),
        }
    }

    #[test]
    fn push_activity_comes_from_commit_system_notes() {
        let notes = [
            note("alice", 10, false, None),
            note("bob", 5, true, Some("commit")),
            note("carol", 1, true, Some("approval")),
        ];
        let last_human = notes.iter().rev().find(|n| !n.system);
        let last_push = notes.iter().rev().find(|n| n.is_push());
        assert_eq!(
            latest_update(&None, last_human, last_push),
            Some((UpdateActivity::Push, "bob".to_string()))
        );
    }

    #[test]
    fn graphql_operation_name_reads_the_query_name() {
        assert_eq!(
            graphql_operation_name(MR_QUERY),
            Some("GlabFeedMergeRequests")
        );
        assert_eq!(
            graphql_operation_name("query GlabFeedCurrentUser { currentUser { username } }"),
            Some("GlabFeedCurrentUser")
        );
        assert_eq!(
            graphql_operation_name("query { currentUser { username } }"),
            None
        );
    }

    #[test]
    fn request_line_formats_outcome() {
        let elapsed = std::time::Duration::from_millis(412);
        assert_eq!(
            request_line("rest todos", false, elapsed, &Ok(vec![0; 10])),
            "rest todos 412ms ok 10B"
        );
        assert_eq!(
            request_line("rest merge_requests", true, elapsed, &Ok(Vec::new())),
            "rest merge_requests paginated 412ms ok 0B"
        );
        let failed: Result<Vec<u8>> = Err(anyhow::anyhow!("glab exited with 1: 502\nBad Gateway"));
        assert_eq!(
            request_line("rest todos", false, elapsed, &failed),
            "rest todos 412ms failed: glab exited with 1: 502 Bad Gateway"
        );
    }

    #[test]
    fn other_system_notes_are_not_pushes() {
        assert!(!note("carol", 1, true, Some("approval")).is_push());
        assert!(!note("alice", 1, false, Some("commit")).is_push());
    }

    fn sample_mr(iid: &str, updated_at: DateTime<Utc>) -> MergeRequest {
        MergeRequest {
            iid: iid.to_string(),
            title: "title".to_string(),
            web_url: "https://example.test".to_string(),
            author_name: "Alice".to_string(),
            author_username: "alice".to_string(),
            approved_by: Vec::new(),
            approved: false,
            draft: false,
            has_conflicts: false,
            notes_count: 2,
            last_note_author: Some("alice".to_string()),
            last_note_at: Some(updated_at),
            ci: CiStatus::None,
            created_at: updated_at,
            updated_at,
            source_branch: "feature".to_string(),
            last_update: Some((UpdateActivity::Push, "alice".to_string())),
        }
    }

    #[test]
    fn unchanged_updated_at_keeps_comment_and_push_activity() {
        let updated_at = Utc::now();
        let previous = sample_mr("12", updated_at);
        let mut fresh = sample_mr("12", updated_at);
        fresh.last_note_author = None;
        fresh.last_note_at = None;
        fresh.last_update = None;

        assert!(!needs_notes(&[previous.clone()], "12", updated_at));
        assert!(needs_notes(&[previous.clone()], "13", updated_at));
        assert!(needs_notes(
            &[previous.clone()],
            "12",
            updated_at + Duration::seconds(1)
        ));

        copy_activity(&[previous], &mut fresh);
        assert_eq!(fresh.last_note_author.as_deref(), Some("alice"));
        assert_eq!(fresh.last_note_at, Some(updated_at));
        assert_eq!(
            fresh.last_update,
            Some((UpdateActivity::Push, "alice".to_string()))
        );
    }

    #[test]
    fn notes_query_loads_iids_in_one_connection() {
        let query = notes_query(&["12", "13"]);
        assert!(query.starts_with("query GlabFeedMergeRequestNotes"));
        assert!(query.contains("mergeRequests(iids: [\"12\", \"13\"], first: 2)"));
        assert!(!query.contains("mr_12:"));
    }
}
