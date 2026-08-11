use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub host: String,
    pub repos: Vec<RepoCfg>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RepoCfg {
    /// Optional tab label; defaults to `path` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Full GitLab project path, e.g. `group/subgroup/repo`.
    pub path: String,
}

impl RepoCfg {
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.path)
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("glab-feed")
        .join("config.toml")
}

pub fn notifications_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("glab-feed")
        .join("notifications.json")
}

const EXAMPLE: &str = r#"host = "git.example.com"

[[repos]]
name = "my-repo"
path = "group/my-repo"

[[repos]]
path = "some-group/another-repo"
"#;

pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        anyhow::bail!(
            "No config found at {}\n\nCreate it with contents like:\n\n{}",
            path.display(),
            EXAMPLE
        );
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing config at {}", path.display()))?;
    if cfg.repos.is_empty() {
        anyhow::bail!("config at {} lists no [[repos]]", path.display());
    }
    Ok(cfg)
}

/// Extract a bare hostname from a variety of GitLab remote URL forms:
/// `git@host:group/repo.git`, `ssh://git@host:22/group/repo`,
/// `https://host/group/repo`, or a bare `host`.
pub fn parse_host(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    // Strip scheme (e.g. https://, ssh://).
    let after_scheme = match s.find("://") {
        Some(idx) => &s[idx + 3..],
        None => s,
    };
    // Strip user@ (e.g. git@).
    let after_user = match after_scheme.split_once('@') {
        Some((_, rest)) => rest,
        None => after_scheme,
    };
    // Host ends at the first ':' (port or scp path) or '/'.
    let host = after_user.split(['/', ':']).next().unwrap_or("");
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_host;

    #[test]
    fn parses_various_remote_urls() {
        let cases = [
            ("git@git.example.com:group/repo.git", "git.example.com"),
            ("ssh://git@git.example.com:22/group/repo.git", "git.example.com"),
            ("https://gitlab.com/group/repo", "gitlab.com"),
            ("https://gitlab.com", "gitlab.com"),
            ("  gitlab.com  ", "gitlab.com"),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_host(input).as_deref(), Some(expected), "input: {input}");
        }
        assert_eq!(parse_host(""), None);
    }
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let toml = toml::to_string_pretty(cfg).context("serializing config")?;
    std::fs::write(&path, toml)
        .with_context(|| format!("writing config at {}", path.display()))?;
    Ok(())
}
