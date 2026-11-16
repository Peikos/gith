use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    /// Canonical SSH base URL (`ssh://git@host:port`) used to give exported
    /// repositories in bulk downloads a working `origin` remote. `None`
    /// means no base URL is configured or detectable; exports then carry no
    /// remote rather than a misleading one.
    pub base_url: Option<String>,
}

impl Config {
    pub const DEFAULT_BRANCH: &str = "main";

    pub fn new(data_dir: Option<PathBuf>) -> Result<Self> {
        let data_dir = match data_dir {
            Some(d) => d,
            None => default_data_dir().context("failed to determine default data directory")?,
        };
        Ok(Self {
            data_dir,
            base_url: None,
        })
    }

    pub fn set_base_url(&mut self, base_url: Option<String>) {
        self.base_url = base_url.map(|u| normalize_base_url(&u));
    }

    /// Canonical SSH URL for a repository path below the server (e.g.
    /// `cs101/lab1/17.git`), if a base URL is configured.
    pub fn repo_url(&self, path: &str) -> Option<String> {
        self.base_url.as_ref().map(|b| format!("{b}/{path}"))
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("gith.db")
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.data_dir.join("repos")
    }

    pub fn classroom_template_path(&self, classroom_slug: &str, repo_name: &str) -> PathBuf {
        self.repos_dir()
            .join(classroom_slug)
            .join(repo_name)
            .with_extension("git")
    }

    pub fn student_repo_path(
        &self,
        classroom_slug: &str,
        repo_name: &str,
        user_id: i64,
    ) -> PathBuf {
        self.repos_dir()
            .join(classroom_slug)
            .join(repo_name)
            .join("students")
            .join(user_id.to_string())
            .with_extension("git")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating data directory {:?}", self.data_dir))?;
        std::fs::create_dir_all(self.repos_dir())
            .with_context(|| format!("creating repos directory {:?}", self.repos_dir()))?;
        Ok(())
    }
}

fn default_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("gith"))
}

/// Normalize a user-supplied base URL: accepts `host`, `host:port`, or a full
/// `ssh://user@host:port` URL; missing scheme and user default to
/// `ssh://git@`. Trailing slashes are stripped.
pub fn normalize_base_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("ssh://git@{trimmed}")
    }
}

pub fn resolve_data_dir<P: AsRef<Path>>(path: P) -> PathBuf {
    let p = path.as_ref();
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_base_url() {
        assert_eq!(
            normalize_base_url("git.example.com"),
            "ssh://git@git.example.com"
        );
        assert_eq!(
            normalize_base_url("git.example.com:2222/"),
            "ssh://git@git.example.com:2222"
        );
        assert_eq!(
            normalize_base_url("ssh://git@git.example.com:2222"),
            "ssh://git@git.example.com:2222"
        );
        assert_eq!(
            normalize_base_url("ssh://gith@git.example.com/"),
            "ssh://gith@git.example.com"
        );
    }

    #[test]
    fn test_repo_url() {
        let mut config = Config::new(Some(PathBuf::from("/tmp/gith-test"))).unwrap();
        assert_eq!(config.repo_url("cs101/lab1/17.git"), None);

        config.set_base_url(Some("git.example.com:2222".to_string()));
        assert_eq!(
            config.repo_url("cs101/lab1/17.git"),
            Some("ssh://git@git.example.com:2222/cs101/lab1/17.git".to_string())
        );
    }
}
