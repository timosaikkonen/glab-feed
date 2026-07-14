use std::collections::HashSet;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::process::Command;

use crate::config::RepoCfg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoFilter {
    All,
    Mine,
    Member,
}

impl RepoFilter {
    /// Extra query parameter for the projects endpoint (if any).
    pub fn query_param(self) -> Option<&'static str> {
        match self {
            RepoFilter::All => None,
            RepoFilter::Mine => Some("owned=true"),
            RepoFilter::Member => Some("membership=true"),
        }
    }

    pub fn next(self) -> Self {
        match self {
            RepoFilter::All => RepoFilter::Mine,
            RepoFilter::Mine => RepoFilter::Member,
            RepoFilter::Member => RepoFilter::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RepoFilter::All => "All",
            RepoFilter::Mine => "Mine",
            RepoFilter::Member => "Member",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub path_with_namespace: String,
    pub name: String,
    pub namespace_full_path: String,
}

// ----- Raw ndjson shape -----

#[derive(Debug, Deserialize)]
struct RawProject {
    path_with_namespace: String,
    name: String,
    namespace: RawNamespace,
}

#[derive(Debug, Deserialize)]
struct RawNamespace {
    full_path: String,
}

/// Fetch projects for the given filter via `glab api ... --output ndjson`.
pub async fn fetch_projects(host: &str, filter: RepoFilter) -> Result<Vec<Project>> {
    let mut endpoint = String::from("projects?simple=true&per_page=100");
    if let Some(param) = filter.query_param() {
        endpoint.push('&');
        endpoint.push_str(param);
    }

    let output = Command::new("glab")
        .arg("api")
        .arg("--hostname")
        .arg(host)
        .arg("--paginate")
        .arg("--output")
        .arg("ndjson")
        .arg(&endpoint)
        .output()
        .await
        .context("failed to spawn `glab`; is it installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("glab exited with {}: {}", output.status, stderr.trim());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut projects: Vec<Project> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let raw: RawProject =
            serde_json::from_str(line).context("parsing project ndjson line")?;
        projects.push(Project {
            path_with_namespace: raw.path_with_namespace,
            name: raw.name,
            namespace_full_path: raw.namespace.full_path,
        });
    }

    sort_projects(&mut projects);
    Ok(projects)
}

fn sort_projects(projects: &mut [Project]) {
    projects.sort_by(|a, b| {
        a.namespace_full_path
            .cmp(&b.namespace_full_path)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// A row to render in the popup list.
pub enum DisplayRow<'a> {
    Group(&'a str),
    Project {
        project: &'a Project,
        selected: bool,
        is_cursor: bool,
    },
}

pub struct RepoSelector {
    pub filter: RepoFilter,
    pub loading: bool,
    pub error: Option<String>,
    pub projects: Vec<Project>,
    pub selected: HashSet<String>,
    pub cursor: usize,
}

impl RepoSelector {
    pub fn new(selected: HashSet<String>) -> Self {
        RepoSelector {
            filter: RepoFilter::Member,
            loading: true,
            error: None,
            projects: Vec::new(),
            selected,
            cursor: 0,
        }
    }

    pub fn set_projects(&mut self, mut projects: Vec<Project>) {
        sort_projects(&mut projects);
        self.projects = projects;
        self.loading = false;
        self.error = None;
        if self.cursor >= self.projects.len() {
            self.cursor = self.projects.len().saturating_sub(1);
        }
    }

    pub fn set_error(&mut self, err: String) {
        self.loading = false;
        self.error = Some(err);
        self.projects.clear();
        self.cursor = 0;
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.projects.len() {
            self.cursor += 1;
        }
    }

    fn cursor_project(&self) -> Option<&Project> {
        self.projects.get(self.cursor)
    }

    pub fn toggle(&mut self) {
        if let Some(p) = self.cursor_project() {
            let path = p.path_with_namespace.clone();
            if !self.selected.remove(&path) {
                self.selected.insert(path);
            }
        }
    }

    /// Grouped display rows: a group header whenever the namespace changes,
    /// followed by its projects.
    pub fn rows(&self) -> Vec<DisplayRow<'_>> {
        let mut rows = Vec::new();
        let mut current_group: Option<&str> = None;
        for (i, p) in self.projects.iter().enumerate() {
            if current_group != Some(p.namespace_full_path.as_str()) {
                current_group = Some(p.namespace_full_path.as_str());
                rows.push(DisplayRow::Group(p.namespace_full_path.as_str()));
            }
            rows.push(DisplayRow::Project {
                project: p,
                selected: self.selected.contains(&p.path_with_namespace),
                is_cursor: i == self.cursor,
            });
        }
        rows
    }

    /// Selected repos as config entries, sorted by path.
    pub fn to_repo_cfgs(&self) -> Vec<RepoCfg> {
        let mut paths: Vec<&String> = self.selected.iter().collect();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let leaf = path.rsplit('/').next().unwrap_or(path).to_string();
                RepoCfg {
                    name: Some(leaf),
                    path: path.clone(),
                }
            })
            .collect()
    }
}
