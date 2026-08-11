use std::collections::HashSet;
use std::time::{Duration, Instant};

use ratatui::widgets::TableState;

use crate::config::RepoCfg;
use crate::gitlab::MergeRequest;
use crate::selector::{Project, RepoFilter, RepoSelector};

pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Latest fetch result for a single repo.
#[derive(Debug, Default, Clone)]
pub struct RepoState {
    pub mrs: Vec<MergeRequest>,
    pub error: Option<String>,
    pub loaded: bool,
}

/// One repo's fetch outcome, sent from the poller.
pub type FetchResult = Result<Vec<MergeRequest>, String>;

/// Heuristic: does this error message look like a missing/invalid auth token?
fn is_auth_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("401") || m.contains("unauthorized") || m.contains("unauthenticated")
}

/// Short, stable label for poll failures (avoids dumping glab stderr into the TUI).
fn shorten_fetch_error(msg: &str) -> String {
    let m = msg.to_lowercase();
    if m.contains("connection refused")
        || m.contains("connection reset")
        || m.contains("no route to host")
        || m.contains("network is unreachable")
        || m.contains("timed out")
        || m.contains("timeout")
        || m.contains("no such host")
        || m.contains("failed to connect")
        || m.contains("could not resolve")
        || m.contains("name or service not known")
    {
        return "GitLab unreachable".to_string();
    }
    if is_auth_error(msg) {
        return "Authentication failed".to_string();
    }
    let line = msg.lines().next().unwrap_or(msg).trim();
    if line.len() > 72 {
        format!("{}…", &line[..69])
    } else {
        line.to_string()
    }
}

/// A full poll cycle's results, each tagged with the repo path it belongs to
/// so results survive repo-set changes without index races.
#[derive(Debug)]
pub struct PollUpdate {
    pub results: Vec<(String, FetchResult)>,
}

/// Lifecycle events from the background poller.
#[derive(Debug)]
pub enum PollEvent {
    Started,
    Finished(PollUpdate),
}

/// A projects fetch result for the repo selector, tagged with its filter so
/// stale responses can be discarded.
#[derive(Debug)]
pub struct SelectorUpdate {
    pub filter: RepoFilter,
    pub result: Result<Vec<Project>, String>,
}

/// What was copied to the clipboard for the footer confirmation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopiedKind {
    Url,
    Ref,
}

impl CopiedKind {
    pub fn message(self) -> &'static str {
        match self {
            CopiedKind::Url => "URL copied!",
            CopiedKind::Ref => "Ref copied!",
        }
    }
}

pub struct App {
    pub host: String,
    pub repos: Vec<RepoCfg>,
    pub repo_states: Vec<RepoState>,
    pub selected_tab: usize,
    pub table_state: TableState,
    pub mine_only: bool,
    pub current_user: String,
    pub next_poll: Instant,
    pub fetching: bool,
    /// Summary error from the most recent poll cycle (any repo failures).
    pub poll_error: Option<String>,
    pub should_quit: bool,
    pub selector: Option<RepoSelector>,
    /// First-run setup: awaiting a GitLab remote URL entry.
    pub awaiting_url: bool,
    pub url_input: String,
    pub url_error: Option<String>,
    /// Set to true during first-run so cancelling setup exits the app.
    pub first_run: bool,
    /// If set, the app should quit and print this error to stderr.
    pub fatal_error: Option<String>,
    /// Incremental filter on MR title / IID in the main table.
    pub mr_query: String,
    pub mr_search_active: bool,
    pub show_help: bool,
    /// When set, the footer shows a copy confirmation until this instant.
    pub copied: Option<(CopiedKind, Instant)>,
    /// Whether `cmux` is on PATH (enables Opt-Enter to open in a split).
    pub cmux_available: bool,
    /// Last `surface_ref` from `cmux --json browser open`, reused when still a browser.
    pub cmux_surface_ref: Option<String>,
}

impl App {
    pub fn new(
        host: String,
        repos: Vec<RepoCfg>,
        current_user: String,
        cmux_available: bool,
    ) -> Self {
        let repo_states = vec![RepoState::default(); repos.len()];
        App {
            host,
            repos,
            repo_states,
            selected_tab: 0,
            table_state: TableState::default(),
            mine_only: false,
            current_user,
            next_poll: Instant::now() + POLL_INTERVAL,
            fetching: true,
            poll_error: None,
            should_quit: false,
            selector: None,
            awaiting_url: false,
            url_input: String::new(),
            url_error: None,
            first_run: false,
            fatal_error: None,
            mr_query: String::new(),
            mr_search_active: false,
            show_help: false,
            copied: None,
            cmux_available,
            cmux_surface_ref: None,
        }
    }

    pub fn show_copied(&mut self, kind: CopiedKind) {
        self.copied = Some((kind, Instant::now() + Duration::from_secs(2)));
    }

    pub fn active_copied(&self) -> Option<CopiedKind> {
        self.copied.and_then(|(kind, until)| {
            if Instant::now() < until {
                Some(kind)
            } else {
                None
            }
        })
    }

    pub fn clear_expired_copied(&mut self) {
        if self.copied.is_some_and(|(_, until)| Instant::now() >= until) {
            self.copied = None;
        }
    }

    // ----- First-run setup -----

    pub fn start_setup(&mut self) {
        self.first_run = true;
        self.awaiting_url = true;
        self.url_input.clear();
        self.url_error = None;
    }

    pub fn push_url_char(&mut self, c: char) {
        self.url_input.push(c);
    }

    pub fn backspace_url(&mut self) {
        self.url_input.pop();
    }

    /// Parse the entered URL into a host. On success, sets `host`, leaves URL
    /// entry, and returns the host so the caller can kick off fetches.
    pub fn submit_url(&mut self) -> Option<String> {
        match crate::config::parse_host(&self.url_input) {
            Some(host) => {
                self.host = host.clone();
                self.awaiting_url = false;
                self.url_error = None;
                Some(host)
            }
            None => {
                self.url_error = Some("Could not parse a hostname from that URL.".to_string());
                None
            }
        }
    }

    pub fn set_current_user(&mut self, user: String) {
        self.current_user = user;
    }

    /// MRs for the current tab, with mine-only and title/IID filters applied.
    pub fn visible_mrs(&self) -> Vec<&MergeRequest> {
        let Some(state) = self.repo_states.get(self.selected_tab) else {
            return Vec::new();
        };
        state
            .mrs
            .iter()
            .filter(|mr| !self.mine_only || mr.author_username == self.current_user)
            .filter(|mr| self.mr_matches_query(mr))
            .collect()
    }

    fn mr_matches_query(&self, mr: &MergeRequest) -> bool {
        if self.mr_query.is_empty() {
            return true;
        }
        let q = self
            .mr_query
            .trim()
            .trim_start_matches('!')
            .to_lowercase();
        mr.iid.to_lowercase().contains(&q) || mr.title.to_lowercase().contains(&q)
    }

    // ----- MR list search -----

    pub fn enter_mr_search(&mut self) {
        self.mr_search_active = true;
    }

    pub fn exit_mr_search(&mut self, clear: bool) {
        self.mr_search_active = false;
        if clear {
            self.mr_query.clear();
        }
        self.reset_selection();
    }

    pub fn push_mr_query_char(&mut self, c: char) {
        self.mr_query.push(c);
        self.reset_selection();
    }

    pub fn backspace_mr_query(&mut self) {
        self.mr_query.pop();
        self.reset_selection();
    }

    pub fn current_state(&self) -> Option<&RepoState> {
        self.repo_states.get(self.selected_tab)
    }

    /// True when the current tab is showing stale MRs after a failed refresh.
    pub fn showing_cached_mrs(&self) -> bool {
        self.current_state()
            .is_some_and(|s| s.error.is_some() && !s.mrs.is_empty())
    }

    pub fn seconds_to_next_poll(&self) -> u64 {
        self.next_poll
            .saturating_duration_since(Instant::now())
            .as_secs()
    }

    pub fn handle_poll_started(&mut self) {
        self.fetching = true;
        self.poll_error = None;
    }

    pub fn handle_poll_finished(&mut self, update: PollUpdate) {
        let mut failures = 0;
        let mut total = 0;
        for (path, result) in update.results.into_iter() {
            total += 1;
            let Some(idx) = self.repos.iter().position(|r| r.path == path) else {
                continue;
            };
            if let Some(state) = self.repo_states.get_mut(idx) {
                state.loaded = true;
                match result {
                    Ok(mrs) => {
                        state.mrs = mrs;
                        state.error = None;
                    }
                    Err(e) => {
                        failures += 1;
                        state.error = Some(shorten_fetch_error(&e));
                    }
                }
            }
        }
        self.fetching = false;
        self.next_poll = Instant::now() + POLL_INTERVAL;
        self.poll_error = if failures > 0 {
            Some(if failures == total {
                "Fetch failed".to_string()
            } else {
                format!("Fetch failed ({failures})")
            })
        } else {
            None
        };
        self.clamp_selection();
    }

    /// Replace the configured repos (e.g. after saving from the selector).
    pub fn set_repos(&mut self, repos: Vec<RepoCfg>) {
        self.repos = repos;
        self.repo_states = vec![RepoState::default(); self.repos.len()];
        if self.selected_tab >= self.repos.len() {
            self.selected_tab = self.repos.len().saturating_sub(1);
        }
        self.next_poll = Instant::now() + POLL_INTERVAL;
        self.reset_selection();
    }

    // ----- Repo selector -----

    pub fn open_selector(&mut self) {
        let selected: HashSet<String> =
            self.repos.iter().map(|r| r.path.clone()).collect();
        self.selector = Some(RepoSelector::new(selected));
    }

    pub fn close_selector(&mut self) {
        self.selector = None;
        // Cancelling setup with nothing configured leaves nothing to show.
        if self.first_run && self.repos.is_empty() {
            self.should_quit = true;
        }
    }

    pub fn apply_selector_update(&mut self, update: SelectorUpdate) {
        if let Some(sel) = self.selector.as_mut() {
            if sel.filter != update.filter {
                return; // stale response for a filter no longer active
            }
            match update.result {
                Ok(projects) => sel.set_projects(projects),
                Err(e) => {
                    if is_auth_error(&e) {
                        self.fatal_error = Some(format!(
                            "GitLab authentication failed for {}.\n\
                             Set a token (e.g. GITLAB_TOKEN) or run `glab auth login`.\n\
                             Details: {e}",
                            self.host
                        ));
                        self.should_quit = true;
                    } else if let Some(sel) = self.selector.as_mut() {
                        sel.set_error(e);
                    }
                }
            }
        }
    }

    pub fn next_tab(&mut self) {
        if !self.repos.is_empty() {
            self.selected_tab = (self.selected_tab + 1) % self.repos.len();
            self.reset_selection();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.repos.is_empty() {
            self.selected_tab = (self.selected_tab + self.repos.len() - 1) % self.repos.len();
            self.reset_selection();
        }
    }

    /// Move the current tab one position left and persist the new order.
    pub fn move_tab_left(&mut self) {
        if self.selected_tab == 0 || self.repos.is_empty() {
            return;
        }
        let i = self.selected_tab;
        self.repos.swap(i, i - 1);
        self.repo_states.swap(i, i - 1);
        self.selected_tab -= 1;
        self.persist_repo_order();
    }

    /// Move the current tab one position right and persist the new order.
    pub fn move_tab_right(&mut self) {
        if self.selected_tab + 1 >= self.repos.len() {
            return;
        }
        let i = self.selected_tab;
        self.repos.swap(i, i + 1);
        self.repo_states.swap(i, i + 1);
        self.selected_tab += 1;
        self.persist_repo_order();
    }

    fn persist_repo_order(&self) {
        let cfg = crate::config::Config {
            host: self.host.clone(),
            repos: self.repos.clone(),
        };
        let _ = crate::config::save(&cfg);
    }

    pub fn select_next(&mut self) {
        let len = self.visible_mrs().len();
        if len == 0 {
            self.table_state.select(None);
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i + 1 < len => i + 1,
            Some(i) => i,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn select_prev(&mut self) {
        let len = self.visible_mrs().len();
        if len == 0 {
            self.table_state.select(None);
            return;
        }
        let i = match self.table_state.selected() {
            Some(0) | None => 0,
            Some(i) => i - 1,
        };
        self.table_state.select(Some(i));
    }

    pub fn toggle_mine(&mut self) {
        self.mine_only = !self.mine_only;
        self.reset_selection();
    }

    /// URL of the currently selected MR, if any.
    pub fn selected_url(&self) -> Option<String> {
        let mrs = self.visible_mrs();
        let idx = self.table_state.selected()?;
        mrs.get(idx).map(|mr| mr.web_url.clone())
    }

    /// Reference form of the currently selected MR (e.g. `!2191`), if any.
    pub fn selected_id(&self) -> Option<String> {
        let mrs = self.visible_mrs();
        let idx = self.table_state.selected()?;
        mrs.get(idx).map(|mr| format!("!{}", mr.iid))
    }

    fn reset_selection(&mut self) {
        if self.visible_mrs().is_empty() {
            self.table_state.select(None);
        } else {
            self.table_state.select(Some(0));
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_mrs().len();
        match self.table_state.selected() {
            _ if len == 0 => self.table_state.select(None),
            Some(i) if i >= len => self.table_state.select(Some(len - 1)),
            None => self.table_state.select(Some(0)),
            _ => {}
        }
    }
}
